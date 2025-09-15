// worker 已经保证这个模块运行在 LocalSet 中
#![allow(clippy::future_not_send)]

use std::{cell::RefCell, rc::Rc, sync::Arc, time::Instant};

use chrono::Utc;
use ferrous_opencc::{OpenCC, config::BuiltinConfig};
use tokio::{
    sync::{
        Mutex as TokioMutex,
        mpsc::{Receiver as TokioReceiver, Sender as TokioSender, channel as tokio_channel},
    },
    task::JoinHandle,
    time::{Duration as TokioDuration, timeout as tokio_timeout},
};
use tokio_util::sync::CancellationToken;
use windows::{
    Foundation::TypedEventHandler,
    Media::{
        Control::{
            GlobalSystemMediaTransportControlsSession as MediaSession,
            GlobalSystemMediaTransportControlsSessionManager as MediaSessionManager,
            GlobalSystemMediaTransportControlsSessionMediaProperties,
            GlobalSystemMediaTransportControlsSessionPlaybackStatus, PlaybackInfoChangedEventArgs,
            TimelinePropertiesChangedEventArgs,
        },
        MediaPlaybackAutoRepeatMode,
    },
    Win32::UI::WindowsAndMessaging::{DispatchMessageW, MSG, TranslateMessage},
    core::{Error as WinError, HSTRING, Result as WinResult},
};

use crate::{
    Controls,
    api::{
        DiagnosticInfo, DiagnosticLevel, NowPlayingInfo, PlaybackStatus, RepeatMode,
        SharedPlayerState, SmtcControlCommand, SmtcSessionInfo, TextConversionMode,
    },
    error::Result,
    tasks, volume_control,
    worker::{InternalCommand, InternalUpdate},
};

const SEEK_DETECTION_THRESHOLD_MS: u64 = 2000;

struct MonitoredSessionGuard {
    session: MediaSession,
    tokens: (i64, i64, i64),
}

impl MonitoredSessionGuard {
    fn new(session: MediaSession, smtc_event_tx: &TokioSender<SmtcEventSignal>) -> WinResult<Self> {
        let session_id = session.SourceAppUserModelId()?.to_string_lossy();
        let tx_media = smtc_event_tx.clone();
        let tx_playback = smtc_event_tx.clone();
        let tx_timeline = smtc_event_tx.clone();

        let media_token = {
            let id = session_id.clone();
            session.MediaPropertiesChanged(&TypedEventHandler::new(move |_, _| {
                let _ = tx_media.try_send(SmtcEventSignal::MediaProperties(id.clone()));
                Ok(())
            }))?
        };

        let playback_token = {
            let id = session_id.clone();
            session.PlaybackInfoChanged(&TypedEventHandler::<
                MediaSession,
                PlaybackInfoChangedEventArgs,
            >::new(move |sender, _| {
                if let Some(session) = &*sender
                    && let Ok(update) = extract_playback_info_from_session(session)
                {
                    let _ = tx_playback.try_send(SmtcEventSignal::PlaybackInfoUpdated(Box::new((
                        id.clone(),
                        update,
                    ))));
                }
                Ok(())
            }))?
        };

        let timeline_token = {
            let id = session_id;
            session.TimelinePropertiesChanged(&TypedEventHandler::<
                MediaSession,
                TimelinePropertiesChangedEventArgs,
            >::new(move |sender, _| {
                if let Some(session) = &*sender
                    && let Ok(timeline_props) = session.GetTimelineProperties()
                    && let Ok(position) = timeline_props.Position()
                {
                    let position_ms = (position.Duration / 10000) as u64;
                    let _ = tx_timeline.try_send(SmtcEventSignal::TimelinePropertiesUpdated(
                        Box::new((id.clone(), position_ms)),
                    ));
                }
                Ok(())
            }))?
        };

        Ok(Self {
            session,
            tokens: (media_token, playback_token, timeline_token),
        })
    }
}

impl Drop for MonitoredSessionGuard {
    fn drop(&mut self) {
        let _ = self
            .session
            .RemoveMediaPropertiesChanged(self.tokens.0)
            .map_err(|e| log::warn!("注销 MediaPropertiesChanged 失败: {e}"));
        let _ = self
            .session
            .RemovePlaybackInfoChanged(self.tokens.1)
            .map_err(|e| log::warn!("注销 PlaybackInfoChanged 失败: {e}"));
        let _ = self
            .session
            .RemoveTimelinePropertiesChanged(self.tokens.2)
            .map_err(|e| log::warn!("注销 TimelinePropertiesChanged 失败: {e}"));
    }
}

struct ManagerEventGuard {
    manager: MediaSessionManager,
    tokens: (i64, i64),
}

impl ManagerEventGuard {
    fn new(
        manager: MediaSessionManager,
        smtc_event_tx: &TokioSender<SmtcEventSignal>,
    ) -> WinResult<Self> {
        let tx_sessions = smtc_event_tx.clone();
        let sessions_token = manager.SessionsChanged(&TypedEventHandler::new(move |_, _| {
            let _ = tx_sessions.try_send(SmtcEventSignal::Sessions);
            Ok(())
        }))?;

        let tx_current = smtc_event_tx.clone();
        let current_session_token =
            manager.CurrentSessionChanged(&TypedEventHandler::new(move |_, _| {
                let _ = tx_current.try_send(SmtcEventSignal::Sessions);
                Ok(())
            }))?;

        Ok(Self {
            manager,
            tokens: (sessions_token, current_session_token),
        })
    }
}

impl Drop for ManagerEventGuard {
    fn drop(&mut self) {
        if let Err(e) = self.manager.RemoveSessionsChanged(self.tokens.0) {
            log::warn!("[ManagerEventGuard] 注销 SessionsChanged 事件失败: {e:?}");
        }
        if let Err(e) = self.manager.RemoveCurrentSessionChanged(self.tokens.1) {
            log::warn!("[ManagerEventGuard] 注销 CurrentSessionChanged 事件失败: {e:?}");
        }
    }
}

/// SMTC 异步操作的通用超时时长。
/// 用于防止 `WinRT` 的异步调用无限期阻塞。
const SMTC_ASYNC_OPERATION_TIMEOUT: TokioDuration = TokioDuration::from_secs(5);

/// Windows API 操作被中止时返回的 HRESULT 错误码 (`E_ABORT`)。
/// 用于在超时发生时手动构造一个错误。
const E_ABORT_HRESULT: windows::core::HRESULT = windows::core::HRESULT(0x8000_4004_u32 as i32);

/// 将 Windows HSTRING 转换为 Rust String。
/// 如果 HSTRING 为空或无效，则返回空 String。
fn hstring_to_string(hstr: &HSTRING) -> String {
    if hstr.is_empty() {
        String::new()
    } else {
        hstr.to_string_lossy()
    }
}

/// 计算封面图片数据的哈希值 (u64)，用于高效地检测封面图片是否发生变化。
pub fn calculate_cover_hash(data: &[u8]) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    data.hash(&mut hasher);
    hasher.finish()
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SmtcEventSignal {
    MediaProperties(String),
    Sessions,
    PlaybackInfoUpdated(Box<(String, PlaybackInfoUpdate)>),
    TimelinePropertiesUpdated(Box<(String, u64)>),
}

#[derive(Debug)]
enum TaskControlSignal {
    ResetSessions,
}

#[derive(Clone)]
struct AppContext {
    state: Rc<RefCell<SmtcState>>,
    player_state_arc: Arc<TokioMutex<SharedPlayerState>>,
    connector_update_tx: TokioSender<InternalUpdate>,
    task_control_tx: TokioSender<TaskControlSignal>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PlaybackInfoUpdate {
    playback_status: PlaybackStatus,
    is_shuffle_active: bool,
    repeat_mode: RepeatMode,
    controls: Controls,
}

/// 封装了 `run_smtc_listener` 主事件循环中所有可变的状态。
struct SmtcState {
    /// RAII Guard，管理当前被监听的会话及其事件处理器。
    session_guard: Option<MonitoredSessionGuard>,
    /// RAII Guard, 用于管理 SMTC 会话管理器及其 `SessionsChanged` 事件。
    manager_guard: Option<ManagerEventGuard>,
    /// 用户通过 `SelectSession` 命令指定的目标会话 ID。
    target_session_id: Option<String>,
    /// 当前的文本转换模式。
    text_conversion_mode: TextConversionMode,
    /// 根据当前模式创建的 `OpenCC` 转换器实例。
    text_converter: Option<OpenCC>,
    /// 当前活动的音量缓动任务的句柄和取消令牌。
    active_volume_easing_task: Option<(JoinHandle<()>, CancellationToken)>,
    active_media_properties_task: Option<JoinHandle<()>>,
    active_cover_fetch_task: Option<JoinHandle<()>>,
    active_progress_timer_task: Option<(JoinHandle<()>, CancellationToken)>,
    is_manager_ready: bool,
    next_easing_task_id: Arc<std::sync::atomic::AtomicU64>,
    last_failed_cover_track: Option<(String, String)>,
    is_apple_music_optimization_enabled: bool,
}

impl SmtcState {
    /// 创建一个新的 `SmtcState` 实例，并初始化所有字段。
    fn new() -> Self {
        Self {
            session_guard: None,
            manager_guard: None,
            target_session_id: None,
            text_conversion_mode: TextConversionMode::default(),
            text_converter: None,
            active_volume_easing_task: None,
            active_media_properties_task: None,
            active_cover_fetch_task: None,
            active_progress_timer_task: None,
            is_manager_ready: false,
            next_easing_task_id: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            last_failed_cover_track: None,
            is_apple_music_optimization_enabled: true,
        }
    }

    fn current_session_id(&self) -> Option<String> {
        self.session_guard
            .as_ref()
            .and_then(|g| g.session.SourceAppUserModelId().ok())
            .map(|h| h.to_string_lossy())
    }
}

struct TrackInfo {
    title: String,
    artist: String,
    album: String,
    duration_ms: u64,
}

fn send_now_playing_update(
    info: NowPlayingInfo,
    connector_update_tx: &TokioSender<InternalUpdate>,
) {
    if connector_update_tx
        .try_send(InternalUpdate::TrackChanged(info))
        .is_err()
    {
        log::warn!("[SMTC] 状态广播失败，所有接收者可能已关闭");
    }
}

/// `SmtcRunner` 封装了 SMTC 事件循环的所有状态和逻辑。
struct SmtcRunner {
    context: AppContext,
    command_executor_handle: Option<JoinHandle<()>>,
    command_executor_tx: Option<TokioSender<SmtcControlCommand>>,
    control_rx: TokioReceiver<InternalCommand>,
    shutdown_rx: TokioReceiver<()>,
    diagnostics_tx: TokioSender<DiagnosticInfo>,
}

impl SmtcRunner {
    async fn run(&mut self) -> Result<()> {
        let (smtc_event_tx, mut smtc_event_rx) = tokio_channel::<SmtcEventSignal>(32);
        let (progress_signal_tx, mut progress_signal_rx) = tokio_channel::<()>(32);
        let (task_control_tx, mut task_control_rx) = tokio_channel::<TaskControlSignal>(8);
        self.context.task_control_tx = task_control_tx.clone();

        let (cmd_tx, mut cmd_rx) = tokio_channel::<SmtcControlCommand>(32);
        self.command_executor_tx = Some(cmd_tx);

        let context_clone = self.context.clone();
        let cmd_handle = tokio::task::spawn_local(async move {
            while let Some(cmd) = cmd_rx.recv().await {
                if let Err(e) =
                    Self::process_media_control_command(context_clone.clone(), cmd).await
                {
                    log::error!("[Command Executor] 执行指令失败: {e:?}");
                }
            }
        });
        self.command_executor_handle = Some(cmd_handle);

        log::debug!("[SmtcRunner] 正在请求 SMTC 管理器...");
        let state_clone = self.context.state.clone();
        let smtc_event_tx_clone = smtc_event_tx.clone();
        tokio::task::spawn_local(async move {
            let async_op = match MediaSessionManager::RequestAsync() {
                Ok(op) => op,
                Err(e) => {
                    log::error!("[SmtcRunner] 启动 SMTC 管理器请求失败: {e:?}");
                    let mut state = state_clone.borrow_mut();
                    Self::on_manager_ready(&mut state, Err(e), &smtc_event_tx_clone);
                    return;
                }
            };

            let final_result = (tokio_timeout(SMTC_ASYNC_OPERATION_TIMEOUT, async_op).await)
                .unwrap_or_else(|_| {
                    log::warn!(
                        "[异步操作] WinRT 异步操作超时 (>{SMTC_ASYNC_OPERATION_TIMEOUT:?})。"
                    );
                    Err(WinError::from(E_ABORT_HRESULT))
                });

            let mut state = state_clone.borrow_mut();
            Self::on_manager_ready(&mut state, final_result, &smtc_event_tx_clone);
        });

        loop {
            pump_pending_messages();

            tokio::select! {
                biased;

                maybe_shutdown = self.shutdown_rx.recv() => {
                    if maybe_shutdown.is_some() {
                        log::info!("[SmtcRunner] 收到关闭信号，准备退出...");
                    } else {
                        log::warn!("[SmtcRunner] 关闭通道已断开，准备退出...");
                    }
                    self.command_executor_tx.take();
                    break Ok(());
                },


                Some(control_signal) = task_control_rx.recv() => {
                    match control_signal {
                        TaskControlSignal::ResetSessions => {
                            if smtc_event_tx.try_send(SmtcEventSignal::Sessions).is_err() {
                                log::warn!("[SmtcRunner] 发送会话重置信号失败，事件通道可能已满或关闭。");
                            }
                        }
                    }
                },

                Some(command) = self.control_rx.recv() => {
                    if let Err(e) = self.handle_internal_command(command, &smtc_event_tx, &progress_signal_tx).await {
                        log::error!("[SmtcRunner] 处理内部命令时发生错误: {e:?}");
                    }
                },

                Some(signal) = smtc_event_rx.recv() => {
                    if let Err(e) = self.handle_smtc_event(signal, &smtc_event_tx).await {
                        log::error!("[SmtcRunner] 处理 SMTC 事件时发生错误: {e:?}");
                    }
                },


                Some(()) = progress_signal_rx.recv() => {
                    self.handle_progress_update_signal().await;
                }
            }
        }
    }

    async fn handle_internal_command(
        &self,
        command: InternalCommand,
        smtc_event_tx: &TokioSender<SmtcEventSignal>,
        progress_signal_tx: &TokioSender<()>,
    ) -> WinResult<()> {
        log::debug!("[SmtcRunner] 收到命令: {command:?}");
        match command {
            InternalCommand::SetTextConversion(mode) => {
                self.on_set_text_conversion(mode, smtc_event_tx).await
            }
            InternalCommand::SelectSmtcSession(id) => {
                self.on_select_session(id, smtc_event_tx);
                Ok(())
            }
            InternalCommand::MediaControl(media_cmd) => {
                if let Some(tx) = &self.command_executor_tx
                    && let Err(e) = tx.try_send(media_cmd)
                {
                    log::error!("[SmtcRunner] 发送指令到执行器失败: {e}");
                }
                Ok(())
            }
            InternalCommand::RequestStateUpdate => {
                self.on_request_state_update(smtc_event_tx).await
            }
            InternalCommand::SetProgressTimer(enabled) => {
                self.on_set_progress_timer(enabled, progress_signal_tx);
                Ok(())
            }
            InternalCommand::SetProgressOffset(offset) => self.on_set_progress_offset(offset).await,
            InternalCommand::SetAppleMusicOptimization(enabled) => {
                self.on_set_apple_music_optimization(enabled, smtc_event_tx)
                    .await
            }
        }
    }

    async fn on_set_text_conversion(
        &self,
        mode: TextConversionMode,
        smtc_event_tx: &TokioSender<SmtcEventSignal>,
    ) -> WinResult<()> {
        let mut text_converter = None;
        let mut should_send_diagnostic = None;
        {
            let mut state = self.context.state.borrow_mut();
            if state.text_conversion_mode == mode {
                return Ok(());
            }

            log::info!("[SmtcRunner] 切换文本转换模式 -> {mode:?}");
            state.text_conversion_mode = mode;

            let config = match mode {
                TextConversionMode::Off => None,
                TextConversionMode::TraditionalToSimplified => Some(BuiltinConfig::T2s),
                TextConversionMode::SimplifiedToTraditional => Some(BuiltinConfig::S2t),
                TextConversionMode::SimplifiedToTaiwan => Some(BuiltinConfig::S2tw),
                TextConversionMode::TaiwanToSimplified => Some(BuiltinConfig::Tw2s),
                TextConversionMode::SimplifiedToHongKong => Some(BuiltinConfig::S2hk),
                TextConversionMode::HongKongToSimplified => Some(BuiltinConfig::Hk2s),
            };

            if let Some(c) = config {
                match OpenCC::from_config(c) {
                    Ok(converter) => text_converter = Some(converter),
                    Err(e) => {
                        should_send_diagnostic =
                            Some(format!("加载 OpenCC 配置 '{c:?}' 失败: {e}"));
                    }
                }
            }
        }

        if let Some(message) = should_send_diagnostic {
            self.send_diagnostic(DiagnosticLevel::Error, message).await;
        }

        let session_id_to_refresh = {
            let mut state = self.context.state.borrow_mut();
            state.text_converter = text_converter;

            state
                .session_guard
                .as_ref()
                .and_then(|g| g.session.SourceAppUserModelId().ok())
                .map(|h| h.to_string_lossy())
        };
        if let Some(id) = session_id_to_refresh {
            let _ = smtc_event_tx.try_send(SmtcEventSignal::MediaProperties(id));
        }
        Ok(())
    }

    fn on_select_session(&self, id: String, smtc_event_tx: &TokioSender<SmtcEventSignal>) {
        let new_target = if id.is_empty() { None } else { Some(id) };
        log::info!("[SmtcRunner] 切换目标会话 -> {new_target:?}");
        self.context.state.borrow_mut().target_session_id = new_target;
        let _ = smtc_event_tx.try_send(SmtcEventSignal::Sessions);
    }

    async fn on_request_state_update(
        &self,
        smtc_event_tx: &TokioSender<SmtcEventSignal>,
    ) -> WinResult<()> {
        {
            let mut state = self.context.state.borrow_mut();
            if !state.is_manager_ready {
                log::warn!("[SmtcRunner] 收到状态更新请求，但 SMTC 管理器尚未就绪，已忽略。");
                return Ok(());
            }
            log::info!("[SmtcRunner] 正在重新获取所有状态...");
            state.session_guard = None;
        }

        self.handle_sessions_changed(smtc_event_tx).await
    }

    fn on_set_progress_timer(&self, enabled: bool, progress_signal_tx: &TokioSender<()>) {
        let mut state = self.context.state.borrow_mut();
        if enabled {
            if state.active_progress_timer_task.is_none() {
                log::info!("[SmtcRunner] 启用进度计时器。");
                let cancel_token = CancellationToken::new();
                let handle = tokio::task::spawn_local(tasks::progress_timer_task(
                    self.context.player_state_arc.clone(),
                    progress_signal_tx.clone(),
                    cancel_token.clone(),
                ));
                state.active_progress_timer_task = Some((handle, cancel_token));
            }
        } else if let Some((task, token)) = state.active_progress_timer_task.take() {
            log::info!("[SmtcRunner] 禁用进度计时器。");
            token.cancel();
            tokio::task::spawn_local(async move {
                let _ = task.await;
            });
        }
    }

    async fn on_set_progress_offset(&self, offset: i64) -> WinResult<()> {
        log::info!("[SmtcRunner] 设置进度偏移量: {offset}ms");
        let mut payload = {
            let mut player_state = self.context.player_state_arc.lock().await;
            player_state.position_offset_ms = offset;
            NowPlayingInfo::from(&*player_state)
        };

        payload.cover_data = None;
        payload.cover_data_hash = None;

        send_now_playing_update(payload, &self.context.connector_update_tx);
        Ok(())
    }

    async fn on_set_apple_music_optimization(
        &self,
        enabled: bool,
        smtc_event_tx: &TokioSender<SmtcEventSignal>,
    ) -> WinResult<()> {
        log::info!("[SmtcRunner] 设置 Apple Music 优化: {enabled}");
        self.context
            .state
            .borrow_mut()
            .is_apple_music_optimization_enabled = enabled;
        self.apply_or_reset_optimizations(smtc_event_tx).await;
        Ok(())
    }

    async fn handle_smtc_event(
        &self,
        signal: SmtcEventSignal,
        smtc_event_tx: &TokioSender<SmtcEventSignal>,
    ) -> WinResult<()> {
        log::trace!("[SmtcRunner] 收到内部事件信号: {signal:?}");
        match signal {
            SmtcEventSignal::Sessions => self.handle_sessions_signal(smtc_event_tx).await,
            SmtcEventSignal::MediaProperties(id) => {
                self.handle_media_properties_signal(id);
                Ok(())
            }
            SmtcEventSignal::PlaybackInfoUpdated(data) => {
                self.handle_playback_info_update(*data).await
            }
            SmtcEventSignal::TimelinePropertiesUpdated(data) => {
                self.handle_timeline_properties_update(*data).await
            }
        }
    }

    async fn handle_sessions_signal(
        &self,
        smtc_event_tx: &TokioSender<SmtcEventSignal>,
    ) -> WinResult<()> {
        if self.context.state.borrow().manager_guard.is_some() {
            self.handle_sessions_changed(smtc_event_tx).await?;
        }
        Ok(())
    }

    fn handle_media_properties_signal(&self, event_session_id: String) {
        let mut state = self.context.state.borrow_mut();

        if state.current_session_id().as_deref() != Some(event_session_id.as_str()) {
            return;
        }

        if let Some(old_task) = state.active_media_properties_task.take() {
            old_task.abort();
        }

        let context_clone = self.context.clone();

        let new_task_handle = tokio::task::spawn_local(async move {
            if let Err(e) =
                Self::process_media_properties_update(context_clone, event_session_id).await
            {
                log::error!("[Media Properties Task] 执行失败: {e:?}");
            }
        });

        state.active_media_properties_task = Some(new_task_handle);
    }

    async fn handle_playback_info_update(
        &self,
        (event_session_id, update): (String, PlaybackInfoUpdate),
    ) -> WinResult<()> {
        if self.context.state.borrow().current_session_id().as_deref()
            != Some(event_session_id.as_str())
        {
            return Ok(());
        }

        let mut payload = {
            let mut state_guard = self.context.player_state_arc.lock().await;
            state_guard.playback_status = update.playback_status;
            state_guard.is_shuffle_active = update.is_shuffle_active;
            state_guard.repeat_mode = update.repeat_mode;
            state_guard.controls = update.controls;
            NowPlayingInfo::from(&*state_guard)
        };

        payload.cover_data = None;
        payload.cover_data_hash = None;

        send_now_playing_update(payload, &self.context.connector_update_tx);

        Ok(())
    }

    async fn handle_timeline_properties_update(
        &self,
        (event_session_id, new_pos_ms): (String, u64),
    ) -> WinResult<()> {
        let mut should_send_update = false;
        let mut payload = {
            let mut state_guard = self.context.player_state_arc.lock().await;

            if self.context.state.borrow().current_session_id().as_deref()
                != Some(event_session_id.as_str())
            {
                return Ok(());
            }

            let raw_estimated_pos_ms = if state_guard.playback_status == PlaybackStatus::Playing
                && let Some(report_time) = state_guard.last_known_position_report_time
            {
                let elapsed_ms = report_time.elapsed().as_millis() as u64;
                state_guard.last_known_position_ms + elapsed_ms
            } else {
                state_guard.last_known_position_ms
            };

            let is_seek = (new_pos_ms as i64 - raw_estimated_pos_ms as i64).abs()
                > SEEK_DETECTION_THRESHOLD_MS as i64;

            if is_seek || new_pos_ms > state_guard.last_known_position_ms {
                state_guard.last_known_position_ms = new_pos_ms;
                state_guard.last_known_position_report_time = Some(Instant::now());
                if state_guard.is_waiting_for_initial_update {
                    state_guard.is_waiting_for_initial_update = false;
                }
                should_send_update = true;
            }
            NowPlayingInfo::from(&*state_guard)
        };
        drop(payload.cover_data);

        if should_send_update {
            payload.cover_data = None;
            payload.cover_data_hash = None;
            send_now_playing_update(payload, &self.context.connector_update_tx);
        }

        Ok(())
    }

    async fn process_media_control_command(
        context: AppContext,
        cmd: SmtcControlCommand,
    ) -> Result<()> {
        log::debug!("[Command Executor] 正在执行命令: {cmd:?}");

        if let SmtcControlCommand::SetVolume(level) = cmd {
            let mut state = context.state.borrow_mut();
            if let Some(guard) = &state.session_guard
                && let Ok(id_hstr) = guard.session.SourceAppUserModelId()
            {
                let session_id_str = hstring_to_string(&id_hstr);
                if !session_id_str.is_empty() {
                    if let Some((old_task, old_token)) = state.active_volume_easing_task.take() {
                        old_token.cancel();
                        tokio::task::spawn_local(async move {
                            let _ = old_task.await;
                        });
                    }
                    let task_id = state
                        .next_easing_task_id
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let (handle, cancel_token) = volume_control::spawn_volume_easing_task(
                        task_id,
                        level,
                        session_id_str,
                        context.connector_update_tx.clone(),
                    );
                    state.active_volume_easing_task = Some((handle, cancel_token));
                }
            }
            return Ok(());
        }

        let command_future = {
            let state = context.state.borrow();
            let Some(guard) = &state.session_guard else {
                log::warn!("[Command Executor] 收到命令 {cmd:?}，但无活动会话，已忽略。");
                return Ok(());
            };

            let async_op_result = match cmd {
                SmtcControlCommand::Play => guard.session.TryPlayAsync(),
                SmtcControlCommand::Pause => guard.session.TryPauseAsync(),
                SmtcControlCommand::SkipNext => guard.session.TrySkipNextAsync(),
                SmtcControlCommand::SkipPrevious => guard.session.TrySkipPreviousAsync(),
                SmtcControlCommand::SeekTo(pos) => guard
                    .session
                    .TryChangePlaybackPositionAsync(pos as i64 * 10000),
                SmtcControlCommand::SetShuffle(is_active) => {
                    guard.session.TryChangeShuffleActiveAsync(is_active)
                }
                SmtcControlCommand::SetRepeatMode(repeat_mode) => {
                    let win_repeat_mode = match repeat_mode {
                        RepeatMode::Off => MediaPlaybackAutoRepeatMode::None,
                        RepeatMode::One => MediaPlaybackAutoRepeatMode::Track,
                        RepeatMode::All => MediaPlaybackAutoRepeatMode::List,
                    };
                    guard.session.TryChangeAutoRepeatModeAsync(win_repeat_mode)
                }
                SmtcControlCommand::SetVolume(_) => unreachable!(),
            };

            async_op_result.ok()
        };

        let Some(future) = command_future else {
            return Ok(());
        };

        let result = tokio_timeout(SMTC_ASYNC_OPERATION_TIMEOUT, future).await;

        match result {
            Ok(Ok(true)) => log::debug!("[Command Executor] 指令 {cmd:?} 成功执行。"),
            Ok(Ok(false)) => {
                log::warn!("[Command Executor] 指令 {cmd:?} 执行失败 (返回 false)。");
            }
            Ok(Err(e)) => log::warn!("[Command Executor] 指令 {cmd:?} 调用失败: {e:?}"),
            Err(_) => log::warn!("[Command Executor] 指令 {cmd:?} 执行超时。"),
        }

        Ok(())
    }

    fn on_manager_ready(
        state: &mut SmtcState,
        result: WinResult<MediaSessionManager>,
        smtc_event_tx: &TokioSender<SmtcEventSignal>,
    ) {
        match result {
            Ok(mgr) => {
                log::trace!("[SmtcRunner] SMTC 管理器已就绪。");
                state.is_manager_ready = true;
                if let Ok(guard) = ManagerEventGuard::new(mgr, smtc_event_tx) {
                    state.manager_guard = Some(guard);
                    let _ = smtc_event_tx.try_send(SmtcEventSignal::Sessions);
                } else {
                    log::error!("[SmtcRunner] 创建 ManagerEventGuard 失败。");
                }
            }
            Err(e) => {
                log::error!("[SmtcRunner] 初始化 SMTC 管理器失败: {e:?}。");
            }
        }
    }

    fn try_parse_apple_music_format(info: TrackInfo) -> TrackInfo {
        const SEPARATOR: &str = "—";
        if !info.artist.contains(SEPARATOR) {
            return info;
        }

        let parts: Vec<&str> = info.artist.split(SEPARATOR).map(str::trim).collect();

        match parts.len() {
            // 模式: "艺术家 — 专辑"
            2 => {
                log::debug!(
                    "检测到 Apple Music 特有的格式(艺术家 — 专辑): '{}', 已拆分",
                    &info.artist
                );
                TrackInfo {
                    artist: parts[0].to_string(),
                    album: parts[1].to_string(),
                    ..info
                }
            }
            // 模式: "艺术家 — 专辑 — 艺术家"
            3 if parts[0] == parts[2] => {
                log::debug!(
                    "检测到 Apple Music 特有的格式(艺术家 — 专辑 — 艺术家): '{}', 已拆分",
                    &info.artist
                );
                TrackInfo {
                    artist: parts[0].to_string(),
                    album: parts[1].to_string(),
                    ..info
                }
            }
            _ => info,
        }
    }

    async fn process_media_properties_update(
        context: AppContext,
        session_id: String,
    ) -> Result<()> {
        let props_future = {
            let state = context.state.borrow();
            let Some(guard) = &state.session_guard else {
                return Ok(());
            };

            let current_id = match guard.session.SourceAppUserModelId().ok() {
                Some(id) => id.to_string_lossy(),
                None => return Ok(()),
            };
            if current_id != session_id {
                return Ok(());
            }

            guard.session.TryGetMediaPropertiesAsync()?
        };

        let props_result = (tokio_timeout(SMTC_ASYNC_OPERATION_TIMEOUT, props_future).await)
            .unwrap_or_else(|_| {
                log::warn!("[Media Properties Task] 获取媒体属性超时。");
                Err(WinError::from(E_ABORT_HRESULT))
            });

        let Ok(props) = props_result else {
            log::warn!(
                "[Media Properties Task] 获取媒体属性失败: {:?}",
                props_result.err()
            );
            return Ok(());
        };

        let session = {
            let state = context.state.borrow();
            let Some(guard) = &state.session_guard else {
                return Ok(());
            };
            guard.session.clone()
        };

        let Some(track_info) = parse_and_convert_properties(&context, &props, &session) else {
            return Ok(());
        };

        let (is_new_track, update_payload) = update_track_state(&context, &track_info).await;

        if context
            .connector_update_tx
            .try_send(InternalUpdate::TrackChanged(update_payload))
            .is_err()
        {
            log::warn!("[SMTC] 状态广播失败，所有接收者可能已关闭");
        }

        let should_fetch_cover = {
            let player_state = context.player_state_arc.lock().await;
            is_new_track || player_state.cover_data_hash.is_none()
        };

        if should_fetch_cover {
            spawn_cover_fetch_task(&context, &props);
        }

        Ok(())
    }

    async fn on_cover_data_ready(context: AppContext, result: WinResult<Option<Vec<u8>>>) {
        match result {
            Ok(Some(bytes)) => {
                let new_hash = calculate_cover_hash(&bytes);
                let should_update;
                let payload = {
                    let mut player_state = context.player_state_arc.lock().await;
                    log::debug!("[State Update] 封面已更新 (大小: {} 字节)。", bytes.len());
                    player_state.cover_data = Some(bytes);
                    player_state.cover_data_hash = Some(new_hash);
                    should_update = true;
                    NowPlayingInfo::from(&*player_state)
                };
                if should_update {
                    send_now_playing_update(payload, &context.connector_update_tx);
                }
            }
            Ok(None) => {
                let mut should_update = false;
                let payload = {
                    let mut player_state = context.player_state_arc.lock().await;
                    if player_state.cover_data.is_some() {
                        log::debug!("[State Update] 清空封面数据。");
                        player_state.cover_data = None;
                        player_state.cover_data_hash = None;
                        should_update = true;
                    }
                    NowPlayingInfo::from(&*player_state)
                };
                if should_update {
                    send_now_playing_update(payload, &context.connector_update_tx);
                }
            }
            Err(e) => {
                log::warn!("[Cover Task] 获取封面失败: {e:?}，重置会话。");
                let payload = {
                    let mut player_state = context.player_state_arc.lock().await;
                    let mut state = context.state.borrow_mut();
                    if !player_state.artist.is_empty() || !player_state.title.is_empty() {
                        state.last_failed_cover_track =
                            Some((player_state.artist.clone(), player_state.title.clone()));
                    }
                    player_state.cover_data = None;
                    player_state.cover_data_hash = None;
                    NowPlayingInfo::from(&*player_state)
                };
                send_now_playing_update(payload, &context.connector_update_tx);

                if let Err(send_err) = context
                    .task_control_tx
                    .try_send(TaskControlSignal::ResetSessions)
                {
                    log::error!("[Cover Task] 发送会话重置信号失败: {send_err}");
                }
            }
        }
    }

    async fn handle_progress_update_signal(&self) {
        let payload_to_send = {
            let player_state = self.context.player_state_arc.lock().await;
            if player_state.playback_status == PlaybackStatus::Playing {
                Some(NowPlayingInfo::from(&*player_state))
            } else {
                None
            }
        };

        if let Some(mut payload) = payload_to_send {
            payload.cover_data = None;
            payload.cover_data_hash = None;
            send_now_playing_update(payload, &self.context.connector_update_tx);
        }
    }

    async fn handle_sessions_changed(
        &self,
        smtc_event_tx: &TokioSender<SmtcEventSignal>,
    ) -> WinResult<()> {
        log::debug!("[会话处理器] 开始处理会话变更...");

        let session_candidates = self.get_sessions().await?;

        let new_session_to_monitor = self.select_session_to_monitor(session_candidates).await?;

        self.update_monitoring_session(new_session_to_monitor, smtc_event_tx)
            .await?;

        Ok(())
    }

    async fn get_sessions(&self) -> WinResult<Vec<(String, MediaSession)>> {
        let sessions_info_list;
        let session_candidates;

        {
            let state_guard = self.context.state.borrow();
            let guard = state_guard
                .manager_guard
                .as_ref()
                .ok_or_else(|| WinError::from(E_ABORT_HRESULT))?;
            let manager = &guard.manager;

            let sessions_ivector = manager.GetSessions()?;
            let mut local_info_list = Vec::new();
            let mut local_candidates = Vec::new();

            for s in sessions_ivector {
                if let Ok(id_hstr) = s.SourceAppUserModelId() {
                    let id_str = hstring_to_string(&id_hstr);
                    if id_str.is_empty() {
                        continue;
                    }
                    local_info_list.push(SmtcSessionInfo {
                        source_app_user_model_id: id_str.clone(),
                        session_id: id_str.clone(),
                        display_name: crate::utils::get_display_name_from_smtc_id(&id_str),
                    });
                    local_candidates.push((id_str, s.clone()));
                }
            }
            sessions_info_list = local_info_list;
            session_candidates = local_candidates;
        }

        if self
            .context
            .connector_update_tx
            .send(InternalUpdate::SmtcSessionListChanged(sessions_info_list))
            .await
            .is_err()
        {
            log::warn!("[会话处理器] 无法发送会话列表更新");
        }

        Ok(session_candidates)
    }

    async fn select_session_to_monitor(
        &self,
        candidates: Vec<(String, MediaSession)>,
    ) -> WinResult<Option<MediaSession>> {
        let manager;
        let target_session_id;

        {
            let state_guard = self.context.state.borrow();

            manager = state_guard
                .manager_guard
                .as_ref()
                .ok_or_else(|| WinError::from(E_ABORT_HRESULT))?
                .manager
                .clone();
            target_session_id = state_guard.target_session_id.clone();
        }

        if let Some(target_id) = target_session_id.as_ref() {
            if let Some((_, session)) = candidates.into_iter().find(|(id, _)| id == target_id) {
                Ok(Some(session))
            } else {
                log::warn!("[会话处理器] 目标会话 '{target_id}' 已消失。");
                let _ = self
                    .context
                    .connector_update_tx
                    .send(InternalUpdate::SelectedSmtcSessionVanished(
                        target_id.clone(),
                    ))
                    .await;
                self.context.state.borrow_mut().target_session_id = None;
                match manager.GetCurrentSession() {
                    Ok(session) => Ok(Some(session)),
                    Err(e) if e.code().is_ok() => Ok(None),
                    Err(e) => Err(e),
                }
            }
        } else {
            match manager.GetCurrentSession() {
                Ok(session) => Ok(Some(session)),
                Err(e) if e.code().is_ok() => Ok(None),
                Err(e) => Err(e),
            }
        }
    }

    async fn notify_active_session_changed(&self, new_session_id: Option<&str>) {
        let pid_for_update = if let Some(id_str) = new_session_id {
            let id_owned = id_str.to_string();
            tokio::task::spawn_blocking(move || volume_control::get_pid_from_identifier(&id_owned))
                .await
                .unwrap_or(None)
        } else {
            None
        };

        if self
            .context
            .connector_update_tx
            .send(InternalUpdate::ActiveSmtcSessionChanged {
                pid: pid_for_update,
            })
            .await
            .is_err()
        {
            log::warn!("[会话处理器] 发送 ActiveSmtcSessionChanged 更新失败。");
        }
    }

    async fn reset_for_session_change(&self) {
        let old_guard = self.context.state.borrow_mut().session_guard.take();
        drop(old_guard);

        let mut player_state = self.context.player_state_arc.lock().await;
        player_state.reset_to_empty();
    }

    async fn initialize_new_session(
        &self,
        new_session: MediaSession,
        smtc_event_tx: &TokioSender<SmtcEventSignal>,
    ) -> WinResult<()> {
        let new_guard = MonitoredSessionGuard::new(new_session.clone(), smtc_event_tx)?;
        self.context.state.borrow_mut().session_guard = Some(new_guard);

        log::info!("[会话处理器] 会话切换完成，获取初始状态...");

        if let Ok(id_hstr) = new_session.SourceAppUserModelId() {
            let _ =
                smtc_event_tx.try_send(SmtcEventSignal::MediaProperties(id_hstr.to_string_lossy()));
        }

        let initial_playback_info = extract_playback_info_from_session(&new_session).ok();
        let initial_timeline_info = (|| -> WinResult<(u64, u64)> {
            let props = new_session.GetTimelineProperties()?;
            let pos = (props.Position()?.Duration / 10000) as u64;
            let dur = (props.EndTime()?.Duration / 10000) as u64;
            Ok((pos, dur))
        })()
        .ok();

        let update_payload = {
            let mut player_state = self.context.player_state_arc.lock().await;
            if let Some(update) = initial_playback_info {
                player_state.playback_status = update.playback_status;
                player_state.is_shuffle_active = update.is_shuffle_active;
                player_state.repeat_mode = update.repeat_mode;
                player_state.controls = update.controls;
            }
            if let Some((pos_ms, dur_ms)) = initial_timeline_info {
                player_state.last_known_position_ms = pos_ms;
                player_state.last_known_position_report_time = Some(Instant::now());
                player_state.song_duration_ms = dur_ms;
            }
            NowPlayingInfo::from(&*player_state)
        };

        send_now_playing_update(update_payload, &self.context.connector_update_tx);

        Ok(())
    }

    async fn handle_no_active_session(&self) {
        log::info!("[会话处理器] 无可用会话，重置状态。");
        let payload = {
            let player_state = self.context.player_state_arc.lock().await;
            NowPlayingInfo::from(&*player_state)
        };
        send_now_playing_update(payload, &self.context.connector_update_tx);
    }

    async fn update_monitoring_session(
        &self,
        new_session_to_monitor: Option<MediaSession>,
        smtc_event_tx: &TokioSender<SmtcEventSignal>,
    ) -> WinResult<()> {
        let new_session_id = new_session_to_monitor
            .as_ref()
            .and_then(|s| s.SourceAppUserModelId().ok())
            .as_ref()
            .map(HSTRING::to_string_lossy);

        if new_session_id == self.context.state.borrow().current_session_id() {
            if let Some(id_str) = new_session_id {
                self.notify_active_session_changed(Some(&id_str)).await;
            }
            return Ok(());
        }

        let current_session_id = self.context.state.borrow().current_session_id();
        log::info!(
            "[会话处理器] 会话切换: {:?} -> {:?}",
            current_session_id.as_deref().unwrap_or("无"),
            new_session_id.as_deref().unwrap_or("无")
        );

        self.reset_for_session_change().await;
        self.notify_active_session_changed(new_session_id.as_deref())
            .await;

        if let Some(new_session) = new_session_to_monitor {
            self.initialize_new_session(new_session, smtc_event_tx)
                .await?;
        } else {
            self.handle_no_active_session().await;
        }

        self.apply_or_reset_optimizations(smtc_event_tx).await;

        Ok(())
    }

    async fn apply_or_reset_optimizations(&self, smtc_event_tx: &TokioSender<SmtcEventSignal>) {
        const APPLE_MUSIC_AUMID_PREFIX: &str = "AppleInc.AppleMusic";

        let is_apple_music = self
            .context
            .state
            .borrow()
            .current_session_id()
            .is_some_and(|id| id.starts_with(APPLE_MUSIC_AUMID_PREFIX));

        let new_offset = if self
            .context
            .state
            .borrow()
            .is_apple_music_optimization_enabled
            && is_apple_music
        {
            -500
        } else {
            0
        };

        let mut should_force_refresh = false;
        let payload = {
            let mut player_state = self.context.player_state_arc.lock().await;
            if player_state.apple_music_optimization_offset_ms != new_offset {
                log::info!("已优化 Apple Music 偏移量 -> {new_offset}ms");
                player_state.apple_music_optimization_offset_ms = new_offset;
                should_force_refresh = true;
            }
            NowPlayingInfo::from(&*player_state)
        };

        if should_force_refresh {
            if let Some(id) = self.context.state.borrow().current_session_id() {
                let _ = smtc_event_tx.try_send(SmtcEventSignal::MediaProperties(id));
            }
            send_now_playing_update(payload, &self.context.connector_update_tx);
        }
    }

    /// 发送一条诊断信息，并同时在控制台打印日志。
    async fn send_diagnostic(&self, level: DiagnosticLevel, message: impl Into<String>) {
        let message = message.into();
        match level {
            DiagnosticLevel::Warning => log::warn!("[诊断信息] {}", &message),
            DiagnosticLevel::Error => log::error!("[诊断信息] {}", &message),
        }

        let info = DiagnosticInfo {
            level,
            message,
            timestamp: Utc::now(),
        };

        if self.diagnostics_tx.send(info).await.is_err() {
            log::warn!("[诊断信息] 诊断通道已关闭，无法发送信息。");
        }
    }
}

fn parse_and_convert_properties(
    context: &AppContext,
    props: &GlobalSystemMediaTransportControlsSessionMediaProperties,
    session: &MediaSession,
) -> Option<TrackInfo> {
    let state = context.state.borrow();
    let get_prop_string = |prop_res: WinResult<HSTRING>, name: &str| {
        prop_res.map_or_else(
            |e| {
                log::warn!("[SmtcRunner] 获取媒体属性 '{name}' 失败: {e:?}");
                String::new()
            },
            |hstr| {
                crate::utils::convert_text(&hstring_to_string(&hstr), state.text_converter.as_ref())
            },
        )
    };

    let title = get_prop_string(props.Title(), "Title");

    const IGNORED_TITLES: &[&str] = &["正在连接…", "Connecting…"];
    if IGNORED_TITLES.iter().any(|&ignored| title == ignored) {
        return None;
    }

    let artist = get_prop_string(props.Artist(), "Artist");
    let album = get_prop_string(props.AlbumTitle(), "AlbumTitle");

    let mut track_info = TrackInfo {
        title,
        artist,
        album,
        duration_ms: 0,
    };

    if state.is_apple_music_optimization_enabled && track_info.album.is_empty() {
        track_info = SmtcRunner::try_parse_apple_music_format(track_info);
    }

    if let Ok(timeline_props) = session.GetTimelineProperties()
        && let Ok(end_time) = timeline_props.EndTime()
    {
        track_info.duration_ms = (end_time.Duration / 10000) as u64;
    }

    Some(track_info)
}

async fn update_track_state(
    context: &AppContext,
    track_info: &TrackInfo,
) -> (bool, NowPlayingInfo) {
    let mut player_state = context.player_state_arc.lock().await;
    let mut state = context.state.borrow_mut();

    let is_new = player_state.title != track_info.title || player_state.artist != track_info.artist;

    if is_new {
        log::info!(
            "[SmtcRunner] 新曲目信息: '{}' - '{}'",
            &track_info.artist,
            &track_info.title
        );

        if state.last_failed_cover_track.as_ref()
            != Some(&(track_info.artist.clone(), track_info.title.clone()))
        {
            state.last_failed_cover_track = None;
        }

        player_state.cover_data = None;
        player_state.cover_data_hash = None;
        player_state.is_waiting_for_initial_update = true;
    }

    player_state.title.clone_from(&track_info.title);
    player_state.artist.clone_from(&track_info.artist);
    player_state.album.clone_from(&track_info.album);
    player_state.song_duration_ms = track_info.duration_ms;

    let update_payload = NowPlayingInfo::from(&*player_state);

    drop(player_state);
    drop(state);

    (is_new, update_payload)
}

fn spawn_cover_fetch_task(
    context: &AppContext,
    props: &GlobalSystemMediaTransportControlsSessionMediaProperties,
) {
    if let Ok(thumb_ref) = props.Thumbnail() {
        let mut state = context.state.borrow_mut();
        if let Some(old_task) = state.active_cover_fetch_task.take() {
            old_task.abort();
        }

        let context_clone = context.clone();

        let cover_task = tokio::task::spawn_local(async move {
            let token = CancellationToken::new();
            let result = tasks::fetch_cover_data_task(thumb_ref, token).await;
            SmtcRunner::on_cover_data_ready(context_clone, result).await;
        });

        state.active_cover_fetch_task = Some(cover_task);
    }
}

fn extract_playback_info_from_session(session: &MediaSession) -> WinResult<PlaybackInfoUpdate> {
    let info = session.GetPlaybackInfo()?;
    let playback_status = match info.PlaybackStatus()? {
        GlobalSystemMediaTransportControlsSessionPlaybackStatus::Playing => PlaybackStatus::Playing,
        GlobalSystemMediaTransportControlsSessionPlaybackStatus::Paused => PlaybackStatus::Paused,
        _ => PlaybackStatus::Stopped,
    };

    let is_shuffle_active = info
        .IsShuffleActive()
        .and_then(|opt| opt.Value())
        .unwrap_or(false);

    let repeat_mode = info
        .AutoRepeatMode()
        .and_then(|opt| opt.Value())
        .map(|mode| match mode {
            MediaPlaybackAutoRepeatMode::Track => RepeatMode::One,
            MediaPlaybackAutoRepeatMode::List => RepeatMode::All,
            _ => RepeatMode::Off,
        })
        .unwrap_or(RepeatMode::Off);

    let c = info.Controls()?;
    let mut controls = Controls::empty();
    let mut add_control = |flag, check: WinResult<bool>| {
        if check.unwrap_or(false) {
            controls.insert(flag);
        }
    };

    add_control(Controls::CAN_PLAY, c.IsPlayEnabled());
    add_control(Controls::CAN_PAUSE, c.IsPauseEnabled());
    add_control(Controls::CAN_SKIP_NEXT, c.IsNextEnabled());
    add_control(Controls::CAN_SKIP_PREVIOUS, c.IsPreviousEnabled());
    add_control(Controls::CAN_SEEK, c.IsPlaybackPositionEnabled());
    add_control(Controls::CAN_CHANGE_SHUFFLE, c.IsShuffleEnabled());
    add_control(Controls::CAN_CHANGE_REPEAT, c.IsRepeatEnabled());

    Ok(PlaybackInfoUpdate {
        playback_status,
        is_shuffle_active,
        repeat_mode,
        controls,
    })
}

pub fn pump_pending_messages() {
    unsafe {
        let mut msg = MSG::default();
        while windows::Win32::UI::WindowsAndMessaging::PeekMessageW(
            &raw mut msg,
            None,
            0,
            0,
            windows::Win32::UI::WindowsAndMessaging::PM_REMOVE,
        )
        .as_bool()
        {
            let _ = TranslateMessage(&raw const msg);
            DispatchMessageW(&raw const msg);
        }
    }
}

pub async fn run_smtc_listener(
    connector_update_tx: TokioSender<InternalUpdate>,
    control_rx: TokioReceiver<InternalCommand>,
    player_state_arc: Arc<TokioMutex<SharedPlayerState>>,
    shutdown_rx: TokioReceiver<()>,
    diagnostics_tx: TokioSender<DiagnosticInfo>,
) -> Result<()> {
    let (task_control_tx, _) = tokio_channel(8);

    let context = AppContext {
        state: Rc::new(RefCell::new(SmtcState::new())),
        player_state_arc,
        connector_update_tx,
        task_control_tx,
    };

    let mut runner = SmtcRunner {
        context,
        command_executor_handle: None,
        command_executor_tx: None,
        control_rx,
        shutdown_rx,
        diagnostics_tx,
    };

    if let Err(e) = runner.run().await {
        log::error!("[SmtcRunner] 事件循环因错误退出: {e:?}");
    }

    if let Some(handle) = runner.command_executor_handle
        && let Err(e) = handle.await
    {
        log::warn!("[SMTC Handler] 等待命令执行器任务时发生错误: {e:?}");
    }

    let tasks_to_clean = {
        let mut state = runner.context.state.borrow_mut();
        (
            state.active_volume_easing_task.take(),
            state.active_cover_fetch_task.take(),
            state.active_media_properties_task.take(),
            state.active_progress_timer_task.take(),
        )
    };

    let (volume_task, cover_task, media_props_task, progress_timer_task) = tasks_to_clean;

    if let Some((task, token)) = volume_task {
        token.cancel();
        let _ = task.await;
    }
    if let Some(task) = cover_task {
        task.abort();
        let _ = task.await;
    }
    if let Some(task) = media_props_task {
        task.abort();
        let _ = task.await;
    }
    if let Some((task, token)) = progress_timer_task {
        token.cancel();
        let _ = task.await;
    }

    log::info!("[SMTC Handler] 监听器任务已完全退出。");
    Ok(())
}
