use std::{future::IntoFuture, sync::Arc, time::Instant};

use chrono::Utc;
use ferrous_opencc::{OpenCC, config::BuiltinConfig};
use tokio::{
    sync::{
        Mutex as TokioMutex, MutexGuard,
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

use windows_future::IAsyncOperation;

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

#[derive(Debug, Clone, PartialEq, Eq)]
struct PlaybackInfoUpdate {
    playback_status: PlaybackStatus,
    is_shuffle_active: bool,
    repeat_mode: RepeatMode,
    controls: Controls,
}

/// 封装了所有可能从后台异步任务返回的结果。
enum AsyncTaskResult {
    /// `MediaSessionManager::RequestAsync` 的结果。
    ManagerReady(WinResult<MediaSessionManager>),
    /// `TryGetMediaPropertiesAsync` 的结果。
    MediaPropertiesReady(
        String,
        WinResult<GlobalSystemMediaTransportControlsSessionMediaProperties>,
    ),
    /// 媒体控制命令（如播放、暂停）的异步执行结果。
    MediaControlCompleted(SmtcControlCommand, WinResult<bool>),
    /// 封面数据获取任务的结果。
    CoverDataReady(WinResult<Option<Vec<u8>>>),
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
    /// 当前活动的封面获取任务的句柄和取消令牌。
    active_cover_fetch_task: Option<(JoinHandle<()>, CancellationToken)>,
    /// 主动进度更新计时器任务的句柄和取消令牌。
    active_progress_timer_task: Option<(JoinHandle<()>, CancellationToken)>,
    /// SMTC 管理器是否已成功初始化。
    is_manager_ready: bool,
    /// 用于为音量缓动任务生成唯一 ID。
    next_easing_task_id: Arc<std::sync::atomic::AtomicU64>,
    last_failed_cover_track: Option<(String, String)>,
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
            active_cover_fetch_task: None,
            active_progress_timer_task: None,
            is_manager_ready: false,
            next_easing_task_id: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            last_failed_cover_track: None,
        }
    }
}

fn spawn_async_op<T, F>(
    async_op_result: WinResult<IAsyncOperation<T>>,
    result_tx: &tokio::sync::mpsc::Sender<AsyncTaskResult>,
    result_mapper: F,
) where
    T: windows::core::RuntimeType + 'static,
    T::Default: 'static,
    IAsyncOperation<T>: IntoFuture<Output = WinResult<T>>,
    F: FnOnce(WinResult<T>) -> AsyncTaskResult + Send + 'static,
{
    if let Ok(async_op) = async_op_result {
        let tx = result_tx.clone();
        tokio::task::spawn_local(async move {
            let result = tokio_timeout(SMTC_ASYNC_OPERATION_TIMEOUT, async_op.into_future()).await;
            let mapped_result = if let Ok(res) = result {
                result_mapper(res)
            } else {
                log::warn!("[异步操作] WinRT 异步操作超时 (>{SMTC_ASYNC_OPERATION_TIMEOUT:?})。");
                result_mapper(Err(WinError::from(E_ABORT_HRESULT)))
            };

            if let Err(e) = tx.send(mapped_result).await {
                log::warn!("[异步操作] 无法将结果发送回主循环: {e}");
            }
        });
    } else if let Err(e) = async_op_result {
        log::warn!("[异步操作] 启动失败: {e:?}");
        if result_tx.try_send(result_mapper(Err(e))).is_err() {
            log::warn!("[异步操作] 启动失败，且无法将错误发送回主循环。");
        }
    }
}

fn send_now_playing_update(
    state_guard: &MutexGuard<SharedPlayerState>,
    connector_update_tx: &TokioSender<InternalUpdate>,
) {
    let latest_info = NowPlayingInfo::from(&**state_guard);

    if connector_update_tx
        .try_send(InternalUpdate::TrackChanged(latest_info))
        .is_err()
    {
        log::warn!("[SMTC] 状态广播失败，所有接收者可能已关闭");
    }
}

/// `SmtcRunner` 封装了 SMTC 事件循环的所有状态和逻辑。
struct SmtcRunner {
    /// 内部状态，如 `session_guard`, `text_converter` 等。
    state: SmtcState,
    /// 用于向 Worker 发送状态更新。
    connector_update_tx: TokioSender<InternalUpdate>,
    /// 共享的播放器状态。
    player_state_arc: Arc<TokioMutex<SharedPlayerState>>,
    control_rx: TokioReceiver<InternalCommand>,
    shutdown_rx: TokioReceiver<()>,
    diagnostics_tx: TokioSender<DiagnosticInfo>,
}

impl SmtcControlCommand {
    fn execute(
        self,
        context: &mut SmtcExecutionContext,
    ) -> Option<WinResult<IAsyncOperation<bool>>> {
        log::debug!("[SmtcRunner] 正在执行命令: {self:?}");

        match self {
            Self::Play => Some(context.session.TryPlayAsync()),
            Self::Pause => Some(context.session.TryPauseAsync()),
            Self::SkipNext => Some(context.session.TrySkipNextAsync()),
            Self::SkipPrevious => Some(context.session.TrySkipPreviousAsync()),
            Self::SeekTo(pos) => Some(
                context
                    .session
                    .TryChangePlaybackPositionAsync(pos as i64 * 10000),
            ),
            Self::SetShuffle(is_active) => {
                Some(context.session.TryChangeShuffleActiveAsync(is_active))
            }
            Self::SetRepeatMode(repeat_mode) => {
                let win_repeat_mode = match repeat_mode {
                    RepeatMode::Off => MediaPlaybackAutoRepeatMode::None,
                    RepeatMode::One => MediaPlaybackAutoRepeatMode::Track,
                    RepeatMode::All => MediaPlaybackAutoRepeatMode::List,
                };
                Some(
                    context
                        .session
                        .TryChangeAutoRepeatModeAsync(win_repeat_mode),
                )
            }
            // 直接执行并返回 None
            Self::SetVolume(level) => {
                if let Ok(id_hstr) = context.session.SourceAppUserModelId() {
                    let session_id_str = hstring_to_string(&id_hstr);
                    if !session_id_str.is_empty() {
                        if let Some((old_task, old_token)) =
                            context.active_volume_easing_task.take()
                        {
                            old_token.cancel();
                            // 启动一个分离的任务来等待旧任务结束，避免阻塞主循环
                            tokio::task::spawn_local(async move {
                                let _ = old_task.await;
                            });
                        }
                        let task_id = context
                            .next_easing_task_id
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let (handle, cancel_token) = volume_control::spawn_volume_easing_task(
                            task_id,
                            level,
                            session_id_str,
                            context.connector_update_tx.clone(),
                        );
                        *context.active_volume_easing_task = Some((handle, cancel_token));
                    }
                }
                None // 没有需要处理的异步操作
            }
        }
    }
}

struct SmtcExecutionContext<'a> {
    session: &'a MediaSession,
    active_volume_easing_task: &'a mut Option<(JoinHandle<()>, CancellationToken)>,
    next_easing_task_id: &'a Arc<std::sync::atomic::AtomicU64>,
    connector_update_tx: &'a TokioSender<InternalUpdate>,
}

impl SmtcRunner {
    async fn run(&mut self) -> Result<()> {
        let (smtc_event_tx, mut smtc_event_rx) = tokio_channel::<SmtcEventSignal>(32);
        let (async_result_tx, mut async_result_rx) = tokio_channel::<AsyncTaskResult>(32);
        let (progress_signal_tx, mut progress_signal_rx) = tokio_channel::<()>(32);

        log::debug!("[SmtcRunner] 正在请求 SMTC 管理器...");
        spawn_async_op(
            MediaSessionManager::RequestAsync(),
            &async_result_tx,
            AsyncTaskResult::ManagerReady,
        );

        loop {
            pump_pending_messages();

            tokio::select! {
                biased;

                maybe_shutdown = self.shutdown_rx.recv() => {
                    if maybe_shutdown.is_some() {
                        log::info!("[SmtcRunner] 收到关闭信号，准备退出...");
                    } else {
                        // recv() 返回 None，意味着发送端已被丢弃
                        log::warn!("[SmtcRunner] 关闭通道已断开，准备退出...");
                    }
                    break Ok(());
                },

                Some(command) = self.control_rx.recv() => {
                    if let Err(e) = self.handle_internal_command(command, &smtc_event_tx, &async_result_tx, &progress_signal_tx).await {
                        log::error!("[SmtcRunner] 处理内部命令时发生致命错误: {e:?}，循环将终止。");
                        return Err(crate::error::SmtcError::Windows(e));
                    }
                },

                Some(signal) = smtc_event_rx.recv() => {
                    self.handle_smtc_event(signal, &smtc_event_tx, &async_result_tx).await?;
                },

                Some(result) = async_result_rx.recv() => {
                    if let Err(e) = self.handle_async_task_result(result, &async_result_tx, &smtc_event_tx).await {
                        log::error!("[SmtcRunner] 处理异步任务结果时发生致命错误: {e:?}，循环将终止。");
                        return Err(e.into());
                    }
                },

                Some(()) = progress_signal_rx.recv() => {
                    self.handle_progress_update_signal().await;
                }
            }
        }
    }

    async fn handle_internal_command(
        &mut self,
        command: InternalCommand,
        smtc_event_tx: &TokioSender<SmtcEventSignal>,
        async_result_tx: &TokioSender<AsyncTaskResult>,
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
                self.on_media_control(media_cmd, async_result_tx);
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
        }
    }

    async fn on_set_text_conversion(
        &mut self,
        mode: TextConversionMode,
        smtc_event_tx: &TokioSender<SmtcEventSignal>,
    ) -> WinResult<()> {
        if self.state.text_conversion_mode == mode {
            return Ok(());
        }

        log::info!("[SmtcRunner] 切换文本转换模式 -> {mode:?}");
        self.state.text_conversion_mode = mode;

        let config = match mode {
            TextConversionMode::Off => None,
            TextConversionMode::TraditionalToSimplified => Some(BuiltinConfig::T2s),
            TextConversionMode::SimplifiedToTraditional => Some(BuiltinConfig::S2t),
            TextConversionMode::SimplifiedToTaiwan => Some(BuiltinConfig::S2tw),
            TextConversionMode::TaiwanToSimplified => Some(BuiltinConfig::Tw2s),
            TextConversionMode::SimplifiedToHongKong => Some(BuiltinConfig::S2hk),
            TextConversionMode::HongKongToSimplified => Some(BuiltinConfig::Hk2s),
        };

        self.state.text_converter = if let Some(config) = config {
            match ferrous_opencc::OpenCC::from_config(config) {
                Ok(converter) => Some(converter),
                Err(e) => {
                    self.send_diagnostic(
                        DiagnosticLevel::Error,
                        format!("加载 OpenCC 配置 '{config:?}' 失败: {e}"),
                    )
                    .await;
                    None
                }
            }
        } else {
            None // TextConversionMode::Off
        };

        if let Some(guard) = &self.state.session_guard
            && let Ok(id_hstr) = guard.session.SourceAppUserModelId()
        {
            let _ =
                smtc_event_tx.try_send(SmtcEventSignal::MediaProperties(id_hstr.to_string_lossy()));
        }

        Ok(())
    }

    fn on_select_session(&mut self, id: String, smtc_event_tx: &TokioSender<SmtcEventSignal>) {
        let new_target = if id.is_empty() { None } else { Some(id) };
        log::info!("[SmtcRunner] 切换目标会话 -> {new_target:?}");
        self.state.target_session_id = new_target;
        let _ = smtc_event_tx.try_send(SmtcEventSignal::Sessions);
    }

    fn on_media_control(
        &mut self,
        media_cmd: SmtcControlCommand,
        async_result_tx: &TokioSender<AsyncTaskResult>,
    ) {
        let Some(guard) = &self.state.session_guard else {
            log::warn!("[SmtcRunner] 收到媒体控制命令 {media_cmd:?}，但没有活动的会话，已忽略。");
            return;
        };

        let mut context = SmtcExecutionContext {
            session: &guard.session,
            active_volume_easing_task: &mut self.state.active_volume_easing_task,
            next_easing_task_id: &self.state.next_easing_task_id,
            connector_update_tx: &self.connector_update_tx,
        };

        if let Some(async_op_result) = media_cmd.execute(&mut context) {
            spawn_async_op(async_op_result, async_result_tx, move |res| {
                AsyncTaskResult::MediaControlCompleted(media_cmd, res)
            });
        }
    }

    async fn on_request_state_update(
        &mut self,
        smtc_event_tx: &TokioSender<SmtcEventSignal>,
    ) -> WinResult<()> {
        if !self.state.is_manager_ready {
            log::warn!("[SmtcRunner] 收到状态更新请求，但 SMTC 管理器尚未就绪，已忽略。");
            return Ok(());
        }
        log::info!("[SmtcRunner] 正在重新获取所有状态...");
        self.state.session_guard = None;
        self.handle_sessions_changed(smtc_event_tx).await
    }

    fn on_set_progress_timer(&mut self, enabled: bool, progress_signal_tx: &TokioSender<()>) {
        if enabled {
            if self.state.active_progress_timer_task.is_none() {
                log::info!("[SmtcRunner] 启用进度计时器。");
                let cancel_token = CancellationToken::new();
                let handle = tokio::task::spawn_local(tasks::progress_timer_task(
                    self.player_state_arc.clone(),
                    progress_signal_tx.clone(),
                    cancel_token.clone(),
                ));
                self.state.active_progress_timer_task = Some((handle, cancel_token));
            }
        } else if let Some((task, token)) = self.state.active_progress_timer_task.take() {
            log::info!("[SmtcRunner] 禁用进度计时器。");
            token.cancel();
            tokio::task::spawn_local(async move {
                let _ = task.await;
            });
        }
    }

    async fn on_set_progress_offset(&self, offset: i64) -> WinResult<()> {
        log::info!("[SmtcRunner] 设置进度偏移量: {offset}ms");
        let mut player_state = self.player_state_arc.lock().await;
        player_state.position_offset_ms = offset;
        send_now_playing_update(&player_state, &self.connector_update_tx);
        drop(player_state);
        Ok(())
    }

    async fn handle_smtc_event(
        &mut self,
        signal: SmtcEventSignal,
        smtc_event_tx: &TokioSender<SmtcEventSignal>,
        async_result_tx: &TokioSender<AsyncTaskResult>,
    ) -> WinResult<()> {
        log::trace!("[SmtcRunner] 收到内部事件信号: {signal:?}");
        match signal {
            SmtcEventSignal::Sessions => self.handle_sessions_signal(smtc_event_tx).await,
            SmtcEventSignal::MediaProperties(id) => {
                self.handle_media_properties_signal(id, async_result_tx);
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
        &mut self,
        smtc_event_tx: &TokioSender<SmtcEventSignal>,
    ) -> WinResult<()> {
        if self.state.manager_guard.is_some() {
            self.handle_sessions_changed(smtc_event_tx).await?;
        }
        Ok(())
    }

    fn handle_media_properties_signal(
        &self,
        event_session_id: String,
        async_result_tx: &TokioSender<AsyncTaskResult>,
    ) {
        let session_clone = self.state.session_guard.as_ref().map_or_else(
            || None,
            |guard| {
                guard.session.SourceAppUserModelId().map_or_else(
                    |_| None,
                    |id_hstr| {
                        if id_hstr.to_string_lossy() == event_session_id {
                            Some(guard.session.clone())
                        } else {
                            None
                        }
                    },
                )
            },
        );

        if let Some(session_clone) = session_clone {
            spawn_async_op(
                session_clone.TryGetMediaPropertiesAsync(),
                async_result_tx,
                move |res| AsyncTaskResult::MediaPropertiesReady(event_session_id, res),
            );
        }
    }

    async fn handle_playback_info_update(
        &self,
        (event_session_id, update): (String, PlaybackInfoUpdate),
    ) -> WinResult<()> {
        let mut state_guard = self.player_state_arc.lock().await;

        if self
            .state
            .session_guard
            .as_ref()
            .and_then(|g| g.session.SourceAppUserModelId().ok())
            .map(|h| h.to_string_lossy())
            .as_deref()
            != Some(event_session_id.as_str())
        {
            return Ok(());
        }

        state_guard.playback_status = update.playback_status;
        state_guard.is_shuffle_active = update.is_shuffle_active;
        state_guard.repeat_mode = update.repeat_mode;
        state_guard.controls = update.controls;

        send_now_playing_update(&state_guard, &self.connector_update_tx);
        drop(state_guard);

        Ok(())
    }

    async fn handle_timeline_properties_update(
        &self,
        (event_session_id, new_pos_ms): (String, u64),
    ) -> WinResult<()> {
        let mut state_guard = self.player_state_arc.lock().await;

        if self
            .state
            .session_guard
            .as_ref()
            .and_then(|g| g.session.SourceAppUserModelId().ok())
            .map(|h| h.to_string_lossy())
            .as_deref()
            != Some(event_session_id.as_str())
        {
            return Ok(());
        }

        let estimated_current_pos_ms = state_guard.get_estimated_current_position_ms();
        let is_seek = (new_pos_ms as i64 - estimated_current_pos_ms as i64).abs()
            > SEEK_DETECTION_THRESHOLD_MS as i64;

        if is_seek || new_pos_ms > state_guard.last_known_position_ms {
            state_guard.last_known_position_ms = new_pos_ms;
            state_guard.last_known_position_report_time = Some(Instant::now());
            if state_guard.is_waiting_for_initial_update {
                state_guard.is_waiting_for_initial_update = false;
            }
            send_now_playing_update(&state_guard, &self.connector_update_tx);
            drop(state_guard);
        }

        Ok(())
    }

    async fn handle_async_task_result(
        &mut self,
        result: AsyncTaskResult,
        async_result_tx: &TokioSender<AsyncTaskResult>,
        smtc_event_tx: &TokioSender<SmtcEventSignal>,
    ) -> WinResult<()> {
        match result {
            AsyncTaskResult::ManagerReady(res) => self.on_manager_ready(res, smtc_event_tx),
            AsyncTaskResult::MediaPropertiesReady(session_id, res) => {
                self.on_media_properties_ready(session_id, res, async_result_tx)
                    .await
            }
            AsyncTaskResult::MediaControlCompleted(cmd, res) => {
                Self::on_media_control_completed(cmd, res);
                Ok(())
            }
            AsyncTaskResult::CoverDataReady(res) => {
                self.on_cover_data_ready(res, smtc_event_tx).await
            }
        }
    }

    fn on_manager_ready(
        &mut self,
        result: WinResult<MediaSessionManager>,
        smtc_event_tx: &TokioSender<SmtcEventSignal>,
    ) -> WinResult<()> {
        match result {
            Ok(mgr) => {
                log::trace!("[SmtcRunner] SMTC 管理器已就绪。");
                self.state.is_manager_ready = true;
                let guard = ManagerEventGuard::new(mgr, smtc_event_tx)?;
                self.state.manager_guard = Some(guard);
                let _ = smtc_event_tx.try_send(SmtcEventSignal::Sessions);
                Ok(())
            }
            Err(e) => {
                log::error!("[SmtcRunner] 初始化 SMTC 管理器失败: {e:?}。");
                Err(e)
            }
        }
    }

    async fn on_media_properties_ready(
        &mut self,
        session_id: String,
        result: WinResult<GlobalSystemMediaTransportControlsSessionMediaProperties>,
        async_result_tx: &TokioSender<AsyncTaskResult>,
    ) -> WinResult<()> {
        let current_session_id = self
            .state
            .session_guard
            .as_ref()
            .and_then(|g| g.session.SourceAppUserModelId().ok())
            .map(|h| h.to_string_lossy());

        if current_session_id != Some(session_id) {
            return Ok(());
        }

        if let Ok(props) = result {
            let get_prop_string = |prop_res: WinResult<HSTRING>, name: &str| {
                prop_res.map_or_else(
                    |e| {
                        log::warn!("[SmtcRunner] 获取媒体属性 '{name}' 失败: {e:?}");
                        String::new()
                    },
                    |hstr| {
                        crate::utils::convert_text(
                            &hstring_to_string(&hstr),
                            self.state.text_converter.as_ref(),
                        )
                    },
                )
            };

            let title = get_prop_string(props.Title(), "Title");
            let artist = get_prop_string(props.Artist(), "Artist");
            let album = get_prop_string(props.AlbumTitle(), "AlbumTitle");

            const IGNORED_TITLES: &[&str] = &["正在连接…", "Connecting…"];
            if IGNORED_TITLES.iter().any(|&ignored| title == ignored) {
                return Ok(());
            }

            let is_new_track;
            {
                let mut player_state = self.player_state_arc.lock().await;

                is_new_track = player_state.title != title || player_state.artist != artist;

                if is_new_track {
                    if self.state.last_failed_cover_track.as_ref()
                        == Some(&(artist.clone(), title.clone()))
                    {
                    } else {
                        self.state.last_failed_cover_track = None;
                    }
                    log::info!(
                        "[SmtcRunner] 接收到新曲目信息: '{}' - '{}'",
                        &artist,
                        &title
                    );
                    player_state.cover_data = None;
                    player_state.cover_data_hash = None;
                    player_state.is_waiting_for_initial_update = true;
                }
                player_state.title = title;
                player_state.artist = artist;
                player_state.album = album;

                send_now_playing_update(&player_state, &self.connector_update_tx);

                drop(player_state);
            }

            if is_new_track
                && self.state.last_failed_cover_track.is_none()
                && let Ok(thumb_ref) = props.Thumbnail()
            {
                if let Some((old_task, old_token)) = self.state.active_cover_fetch_task.take() {
                    old_token.cancel();
                    tokio::task::spawn_local(async move {
                        let _ = old_task.await;
                        log::debug!("[SmtcRunner] 旧的封面获取任务已结束。");
                    });
                    log::debug!("[SmtcRunner] 已取消旧的封面获取任务");
                }
                let async_result_tx_clone = async_result_tx.clone();
                let cancel_token = CancellationToken::new();
                let token_for_task = cancel_token.clone();

                let cover_task = tokio::task::spawn_local(async move {
                    let result = tasks::fetch_cover_data_task(thumb_ref, token_for_task).await;
                    if async_result_tx_clone
                        .send(AsyncTaskResult::CoverDataReady(result))
                        .await
                        .is_err()
                    {
                        log::warn!("[Cover Fetcher] 无法将封面结果发送回主循环，通道已关闭。");
                    }
                });
                self.state.active_cover_fetch_task = Some((cover_task, cancel_token));
            }
        } else if let Err(e) = result {
            log::warn!("[SmtcRunner] 获取媒体属性失败: {e:?}");
        }
        Ok(())
    }

    fn on_media_control_completed(cmd: SmtcControlCommand, res: WinResult<bool>) {
        match res {
            Ok(true) => log::debug!("[SmtcRunner] 媒体控制指令 {cmd:?} 成功执行。"),
            Ok(false) => log::warn!("[SmtcRunner] 媒体控制指令 {cmd:?} 执行失败 (返回 false)。"),
            Err(e) => log::warn!("[SmtcRunner] 媒体控制指令 {cmd:?} 调用失败: {e:?}"),
        }
    }

    async fn on_cover_data_ready(
        &mut self,
        result: WinResult<Option<Vec<u8>>>,
        smtc_event_tx: &TokioSender<SmtcEventSignal>,
    ) -> WinResult<()> {
        match result {
            Ok(Some(bytes)) => {
                let new_hash = calculate_cover_hash(&bytes);
                let mut player_state = self.player_state_arc.lock().await;
                if player_state.cover_data_hash != Some(new_hash) {
                    log::debug!("[State Update] 封面已更新 (大小: {} 字节)。", bytes.len());
                    player_state.cover_data = Some(bytes);
                    player_state.cover_data_hash = Some(new_hash);
                    send_now_playing_update(&player_state, &self.connector_update_tx);
                    drop(player_state);
                }
            }
            Ok(None) => {
                let mut player_state = self.player_state_arc.lock().await;
                if player_state.cover_data.is_some() {
                    log::debug!("[State Update] 清空封面数据。");
                    player_state.cover_data = None;
                    player_state.cover_data_hash = None;
                    send_now_playing_update(&player_state, &self.connector_update_tx);
                    drop(player_state);
                }
            }
            Err(e) => {
                log::warn!("[SmtcRunner] 获取封面失败: {e:?}，正在重置会话。");
                {
                    let mut player_state = self.player_state_arc.lock().await;

                    if !player_state.artist.is_empty() || !player_state.title.is_empty() {
                        self.state.last_failed_cover_track =
                            Some((player_state.artist.clone(), player_state.title.clone()));
                    }

                    player_state.cover_data = None;
                    player_state.cover_data_hash = None;
                }
                let player_state = self.player_state_arc.lock().await;
                send_now_playing_update(&player_state, &self.connector_update_tx);

                self.state.session_guard = None;

                if let Err(e) = smtc_event_tx.try_send(SmtcEventSignal::Sessions) {
                    log::error!("[SmtcRunner] 发送会话重置信号失败: {e:?}");
                }
            }
        }
        Ok(())
    }

    async fn handle_progress_update_signal(&self) {
        let mut should_send_update = false;
        let mut is_currently_playing = false;

        if let Some(guard) = &self.state.session_guard {
            let unwind_result = std::panic::catch_unwind(|| {
                guard.session.GetPlaybackInfo().ok().and_then(|info| {
                    info.PlaybackStatus().ok().map(|status| match status {
                        GlobalSystemMediaTransportControlsSessionPlaybackStatus::Playing => {
                            PlaybackStatus::Playing
                        }
                        GlobalSystemMediaTransportControlsSessionPlaybackStatus::Paused => {
                            PlaybackStatus::Paused
                        }
                        _ => PlaybackStatus::Stopped,
                    })
                })
            });

            if let Ok(Some(current_status)) = unwind_result {
                let mut player_state = self.player_state_arc.lock().await;
                if player_state.playback_status != current_status {
                    player_state.playback_status = current_status;
                    should_send_update = true;
                }
                is_currently_playing = player_state.playback_status == PlaybackStatus::Playing;
                drop(player_state);
            }
        } else {
            let mut player_state = self.player_state_arc.lock().await;
            if player_state.playback_status != PlaybackStatus::Stopped {
                should_send_update = true;
                player_state.playback_status = PlaybackStatus::Stopped;
            }
        }

        if is_currently_playing || should_send_update {
            let player_state = self.player_state_arc.lock().await;
            send_now_playing_update(&player_state, &self.connector_update_tx);
        }
    }

    async fn handle_sessions_changed(
        &mut self,
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
        let guard = self
            .state
            .manager_guard
            .as_ref()
            .ok_or_else(|| WinError::from(E_ABORT_HRESULT))?;
        let manager = &guard.manager;

        let sessions_ivector = manager.GetSessions()?;
        let mut sessions_info_list = Vec::new();
        let mut session_candidates = Vec::new();

        for s in sessions_ivector {
            if let Ok(id_hstr) = s.SourceAppUserModelId() {
                let id_str = hstring_to_string(&id_hstr);
                if id_str.is_empty() {
                    continue;
                }
                sessions_info_list.push(SmtcSessionInfo {
                    source_app_user_model_id: id_str.clone(),
                    session_id: id_str.clone(),
                    display_name: crate::utils::get_display_name_from_smtc_id(&id_str),
                });
                session_candidates.push((id_str, s.clone()));
            }
        }
        if self
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
        &mut self,
        candidates: Vec<(String, MediaSession)>,
    ) -> WinResult<Option<MediaSession>> {
        let guard = self
            .state
            .manager_guard
            .as_ref()
            .ok_or_else(|| WinError::from(E_ABORT_HRESULT))?;
        let manager = &guard.manager;

        if let Some(target_id) = self.state.target_session_id.as_ref() {
            if let Some((_, session)) = candidates.into_iter().find(|(id, _)| id == target_id) {
                Ok(Some(session))
            } else {
                log::warn!("[会话处理器] 目标会话 '{target_id}' 已消失。");
                let _ = self
                    .connector_update_tx
                    .send(InternalUpdate::SelectedSmtcSessionVanished(
                        target_id.clone(),
                    ))
                    .await;
                self.state.target_session_id = None;
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

    async fn update_monitoring_session(
        &mut self,
        new_session_to_monitor: Option<MediaSession>,
        smtc_event_tx: &TokioSender<SmtcEventSignal>,
    ) -> WinResult<()> {
        let new_session_id_hstring = new_session_to_monitor
            .as_ref()
            .and_then(|s| s.SourceAppUserModelId().ok());

        let new_session_id = new_session_id_hstring
            .as_ref()
            .map(HSTRING::to_string_lossy);

        let current_session_id = self
            .state
            .session_guard
            .as_ref()
            .and_then(|g| g.session.SourceAppUserModelId().ok())
            .map(|h| h.to_string_lossy());

        if new_session_id == current_session_id {
            if let Some(id_str) = new_session_id {
                let pid = volume_control::get_pid_from_identifier(&id_str);
                let _ = self
                    .connector_update_tx
                    .send(InternalUpdate::ActiveSmtcSessionChanged { pid })
                    .await;
            }
            return Ok(());
        }

        let old_guard = self.state.session_guard.take();
        drop(old_guard);

        log::info!(
            "[会话处理器] 检测到会话切换: 从 {:?} -> 到 {:?}",
            current_session_id.as_deref().unwrap_or("无"),
            new_session_id.as_deref().unwrap_or("无")
        );

        let pid_for_update = if let Some(id_str) = new_session_id.as_deref() {
            let id_owned = id_str.to_string();
            tokio::task::spawn_blocking(move || volume_control::get_pid_from_identifier(&id_owned))
                .await
                .unwrap_or(None)
        } else {
            None
        };

        if self
            .connector_update_tx
            .send(InternalUpdate::ActiveSmtcSessionChanged {
                pid: pid_for_update,
            })
            .await
            .is_err()
        {
            log::warn!("[会话处理器] 发送 ActiveSmtcSessionChanged 更新失败。");
        }

        {
            let mut player_state = self.player_state_arc.lock().await;
            player_state.reset_to_empty();
        }

        if let Some(new_s) = new_session_to_monitor {
            let new_guard = MonitoredSessionGuard::new(new_s.clone(), smtc_event_tx)?;
            self.state.session_guard = Some(new_guard);

            log::info!("[会话处理器] 会话切换完成，正在获取初始状态。");
            if let Some(id_hstr) = new_session_id_hstring {
                let _ = smtc_event_tx
                    .try_send(SmtcEventSignal::MediaProperties(id_hstr.to_string_lossy()));
            }

            let initial_playback_info = extract_playback_info_from_session(&new_s).ok();
            let initial_timeline_info = (|| -> WinResult<(u64, u64)> {
                let props = new_s.GetTimelineProperties()?;
                let pos = (props.Position()?.Duration / 10000) as u64;
                let dur = (props.EndTime()?.Duration / 10000) as u64;
                Ok((pos, dur))
            })()
            .ok();

            {
                let mut player_state = self.player_state_arc.lock().await;
                if let Some(update) = initial_playback_info {
                    player_state.playback_status = update.playback_status;
                    player_state.is_shuffle_active = update.is_shuffle_active;
                    player_state.repeat_mode = update.repeat_mode;
                    player_state.controls = update.controls;
                }
                if let Some((pos_ms, dur_ms)) = initial_timeline_info {
                    player_state.last_known_position_ms = pos_ms;
                    player_state.last_known_position_report_time = Some(Instant::now());
                    if dur_ms > 0 {
                        player_state.song_duration_ms = dur_ms;
                    }
                }
                send_now_playing_update(&player_state, &self.connector_update_tx);
                drop(player_state);
            }
        } else {
            log::info!("[会话处理器] 没有可用的媒体会话，已重置状态。");
            let player_state = self.player_state_arc.lock().await;
            send_now_playing_update(&player_state, &self.connector_update_tx);
        }

        Ok(())
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
    if c.IsPlayEnabled().unwrap_or(false) {
        controls.insert(Controls::CAN_PLAY);
    }
    if c.IsPauseEnabled().unwrap_or(false) {
        controls.insert(Controls::CAN_PAUSE);
    }
    if c.IsNextEnabled().unwrap_or(false) {
        controls.insert(Controls::CAN_SKIP_NEXT);
    }
    if c.IsPreviousEnabled().unwrap_or(false) {
        controls.insert(Controls::CAN_SKIP_PREVIOUS);
    }
    if c.IsPlaybackPositionEnabled().unwrap_or(false) {
        controls.insert(Controls::CAN_SEEK);
    }
    if c.IsShuffleEnabled().unwrap_or(false) {
        controls.insert(Controls::CAN_CHANGE_SHUFFLE);
    }
    if c.IsRepeatEnabled().unwrap_or(false) {
        controls.insert(Controls::CAN_CHANGE_REPEAT);
    }

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
    let mut runner = SmtcRunner {
        state: SmtcState::new(),
        connector_update_tx,
        player_state_arc,
        control_rx,
        shutdown_rx,
        diagnostics_tx,
    };

    if let Err(e) = runner.run().await {
        log::error!("[SmtcRunner] 事件循环因错误退出: {e:?}");
    }

    let state = runner.state;
    if let Some((task, token)) = state.active_volume_easing_task {
        token.cancel();
        let _ = task.await;
    }
    if let Some((task, token)) = state.active_cover_fetch_task {
        token.cancel();
        let _ = task.await;
    }
    if let Some((task, token)) = state.active_progress_timer_task {
        token.cancel();
        let _ = task.await;
    }

    log::info!("[SMTC Handler] 监听器任务已完全退出。");
    Ok(())
}
