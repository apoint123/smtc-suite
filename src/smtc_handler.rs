use std::{future::IntoFuture, sync::Arc, time::Instant};

use chrono::Utc;
use easer::functions::Easing;
use ferrous_opencc::{OpenCC, config::BuiltinConfig};
use tokio::{
    sync::{
        Mutex as TokioMutex, MutexGuard,
        mpsc::{Receiver as TokioReceiver, Sender as TokioSender, channel as tokio_channel},
        watch,
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
            GlobalSystemMediaTransportControlsSessionPlaybackStatus,
        },
        MediaPlaybackAutoRepeatMode,
    },
    Storage::Streams::{Buffer, DataReader, IRandomAccessStreamReference, InputStreamOptions},
    Win32::UI::WindowsAndMessaging::{DispatchMessageW, MSG, TranslateMessage},
    core::{Error as WinError, HSTRING, Result as WinResult},
};

use windows_core::Interface;
use windows_future::{IAsyncInfo, IAsyncOperation};

use crate::{
    api::{
        DiagnosticInfo, DiagnosticLevel, NowPlayingInfo, RepeatMode, SmtcControlCommand,
        SmtcSessionInfo, TextConversionMode,
    },
    error::Result,
    volume_control,
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
            session.PlaybackInfoChanged(&TypedEventHandler::new(move |_, _| {
                let _ = tx_playback.try_send(SmtcEventSignal::PlaybackInfo(id.clone()));
                Ok(())
            }))?
        };

        let timeline_token = {
            let id = session_id.clone();
            session.TimelinePropertiesChanged(&TypedEventHandler::new(move |_, _| {
                let _ = tx_timeline.try_send(SmtcEventSignal::TimelineProperties(id.clone()));
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
    token: i64,
}

impl ManagerEventGuard {
    fn new(
        manager: MediaSessionManager,
        smtc_event_tx: &TokioSender<SmtcEventSignal>,
    ) -> WinResult<Self> {
        let tx = smtc_event_tx.clone();
        let token = manager.SessionsChanged(&TypedEventHandler::new(move |_, _| {
            let _ = tx.try_send(SmtcEventSignal::Sessions);
            Ok(())
        }))?;
        Ok(Self { manager, token })
    }
}

impl Drop for ManagerEventGuard {
    fn drop(&mut self) {
        if let Err(e) = self.manager.RemoveSessionsChanged(self.token) {
            log::warn!("[ManagerEventGuard] 注销 SessionsChanged 事件失败: {e:?}");
        }
    }
}

/// SMTC 异步操作的通用超时时长。
/// 用于防止 `WinRT` 的异步调用无限期阻塞。
const SMTC_ASYNC_OPERATION_TIMEOUT: TokioDuration = TokioDuration::from_secs(5);

/// Windows API 操作被中止时返回的 HRESULT 错误码 (`E_ABORT`)。
/// 用于在超时发生时手动构造一个错误。
const E_ABORT_HRESULT: windows::core::HRESULT = windows::core::HRESULT(0x8000_4004_u32 as i32);

/// 音量缓动动画的总时长（毫秒）。
const VOLUME_EASING_DURATION_MS: f32 = 250.0;

/// 音量缓动动画的总步数。
const VOLUME_EASING_STEPS: u32 = 15;

/// 音量变化小于此阈值时，不执行缓动动画，直接设置最终值。
const VOLUME_EASING_THRESHOLD: f32 = 0.01;

/// 允许获取的封面图片的最大字节数，防止过大的图片消耗过多内存。
const MAX_COVER_SIZE_BYTES: usize = 20_971_520; // 20 MB

/// SMTC Handler 内部用于聚合和管理当前播放状态的可变结构。
///
/// 这个结构体被一个 `Arc<TokioMutex<...>>` 包裹，在多个异步任务之间安全共享。
/// “状态管理器 Actor” 任务是唯一有权直接修改此结构体的地方。
#[derive(Debug, Clone, Default)]
pub(crate) struct SharedPlayerState {
    /// 歌曲标题。
    pub title: String,
    /// 艺术家。
    pub artist: String,
    /// 专辑。
    pub album: String,
    /// 歌曲总时长（毫秒）。
    pub song_duration_ms: u64,
    /// 上次从 SMTC 获取到的播放位置（毫秒）。
    pub last_known_position_ms: u64,
    /// `last_known_position_ms` 被更新时的时间点。
    pub last_known_position_report_time: Option<Instant>,
    /// 当前是否正在播放。
    pub is_playing: bool,
    /// SMTC 报告是否支持播放操作。
    pub can_play: bool,
    /// SMTC 报告是否支持暂停操作。
    pub can_pause: bool,
    /// SMTC 报告是否支持下一首操作。
    pub can_skip_next: bool,
    /// SMTC 报告是否支持上一首操作。
    pub can_skip_previous: bool,
    /// SMTC 报告是否支持跳转操作。
    pub can_seek: bool,
    /// SMTC 报告是否支持随机播放切换。
    pub can_change_shuffle: bool,
    /// SMTC 报告是否支持循环模式切换。
    pub can_change_repeat: bool,
    /// 当前是否处于随机播放模式。
    pub is_shuffle_active: bool,
    /// 当前的重复播放模式。
    pub repeat_mode: RepeatMode,
    pub cover_data: Option<Vec<u8>>,
    pub cover_data_hash: Option<u64>,
    /// 一个标志，表示正在等待来自SMTC的第一次更新。
    /// 在此期间，应暂停进度计时器。
    pub is_waiting_for_initial_update: bool,
}

impl SharedPlayerState {
    /// 将播放状态重置为空白/默认状态。
    /// 通常在没有活动媒体会话时调用。
    pub fn reset_to_empty(&mut self) {
        *self = Self {
            is_waiting_for_initial_update: true,
            ..Self::default()
        };
    }

    /// 根据上次报告的播放位置和当前时间，估算实时的播放进度。
    pub fn get_estimated_current_position_ms(&self) -> u64 {
        if self.is_playing
            && let Some(report_time) = self.last_known_position_report_time
        {
            let elapsed_ms = report_time.elapsed().as_millis() as u64;
            let estimated_pos = self.last_known_position_ms + elapsed_ms;
            // 确保估算的位置不超过歌曲总时长（如果时长有效）
            if self.song_duration_ms > 0 {
                return estimated_pos.min(self.song_duration_ms);
            }
            return estimated_pos;
        }
        self.last_known_position_ms
    }
}

/// 实现从内部共享状态到公共 `NowPlayingInfo` 的转换。
impl From<&SharedPlayerState> for NowPlayingInfo {
    fn from(state: &SharedPlayerState) -> Self {
        Self {
            title: Some(state.title.clone()),
            artist: Some(state.artist.clone()),
            album_title: Some(state.album.clone()),
            duration_ms: Some(state.song_duration_ms),
            position_ms: Some(state.get_estimated_current_position_ms()),
            is_playing: Some(state.is_playing),
            is_shuffle_active: Some(state.is_shuffle_active),
            repeat_mode: Some(state.repeat_mode),
            position_report_time: state.last_known_position_report_time,
            can_play: Some(state.can_play),
            can_pause: Some(state.can_pause),
            can_skip_next: Some(state.can_skip_next),
            can_skip_previous: Some(state.can_skip_previous),
            can_seek: Some(state.can_seek),
            can_change_shuffle: Some(state.can_change_shuffle),
            can_change_repeat: Some(state.can_change_repeat),
            cover_data: state.cover_data.clone(),
            cover_data_hash: state.cover_data_hash,
        }
    }
}

/// 将 Windows HSTRING 转换为 Rust String。
/// 如果 HSTRING 为空或无效，则返回空 String。
fn hstring_to_string(hstr: &HSTRING) -> String {
    if hstr.is_empty() {
        String::new()
    } else {
        hstr.to_string_lossy()
    }
}

/// 使用超时来执行一个 `WinRT` 异步操作。
/// 如果超时，会尝试取消该操作。
async fn run_winrt_op_with_timeout<F, T>(operation: F) -> WinResult<T>
where
    T: windows::core::RuntimeType + 'static,
    T::Default: 'static,
    F: IntoFuture<Output = WinResult<T>> + Interface + Clone,
{
    match tokio_timeout(
        SMTC_ASYNC_OPERATION_TIMEOUT,
        operation.clone().into_future(),
    )
    .await
    {
        // 异步操作在超时前成功完成
        Ok(Ok(result)) => Ok(result),
        // 异步操作在超时前完成，但自身返回了错误
        Ok(Err(e)) => Err(e),
        // tokio_timeout 返回超时错误
        Err(_) => {
            log::warn!("WinRT 异步操作超时 (>{SMTC_ASYNC_OPERATION_TIMEOUT:?}).");

            if let Ok(async_info) = operation.cast::<IAsyncInfo>() {
                if let Err(e) = async_info.Cancel() {
                    log::warn!("取消 WinRT 异步操作失败: {e:?}");
                }
            } else {
                log::warn!("无法将异步操作转换为 IAsyncInfo 来执行取消操作。");
            }

            Err(WinError::from(E_ABORT_HRESULT))
        }
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
    PlaybackInfo(String),
    TimelineProperties(String),
    Sessions,
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
        }
    }
}

/// 一个通用的辅助函数，用于将返回 `IAsyncOperation` 的 `WinRT` API 调用派发到 Tokio 的本地任务中执行。
///
/// 它处理超时逻辑，并将最终结果（成功或失败）通过通道发送回主循环进行统一处理。
///
/// # 参数
/// * `async_op_result`: 一个 `WinResult`，其中应包含了要执行的 `IAsyncOperation`。
/// * `result_tx`: 用于发送异步任务结果的 Tokio 通道发送端。
/// * `result_mapper`: 一个闭包，用于将 `IAsyncOperation` 的最终结果 (`WinResult<T>`) 包装成 `AsyncTaskResult` 枚举。
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
        // 使用 spawn_local 至关重要，因为它不要求 Future 是 `Send`。
        // WinRT 的代理对象不是 `Send` 的，因此不能在多线程运行时中被 `.await`。
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
        // 如果创建 IAsyncOperation 本身就失败了，直接将错误打包发送回去。
        log::warn!("[异步操作] 启动失败: {e:?}");
        if result_tx.try_send(result_mapper(Err(e))).is_err() {
            log::warn!("[异步操作] 启动失败，且无法将错误发送回主循环。");
        }
    }
}

fn send_now_playing_update(
    state_guard: &MutexGuard<SharedPlayerState>,
    now_playing_tx: &watch::Sender<NowPlayingInfo>,
) {
    let latest_info = NowPlayingInfo::from(&**state_guard);

    if now_playing_tx.send(latest_info).is_err() {
        log::warn!("[SMTC] 状态广播失败，所有接收者可能已关闭");
    }
}

/// 从 SMTC 会话中获取封面图片数据。
async fn fetch_cover_data_task(
    thumb_ref: IRandomAccessStreamReference,
    cancel_token: CancellationToken,
) -> WinResult<Option<Vec<u8>>> {
    let start_time = Instant::now();
    log::debug!("[Cover Fetcher] 正在获取封面数据...");

    let result = tokio::select! {
        biased;
        () = cancel_token.cancelled() => {
            log::debug!("[Cover Fetcher] 任务被协作式取消。");
            return Err(WinError::from(E_ABORT_HRESULT));
        }
        res = async {
            let stream = run_winrt_op_with_timeout(thumb_ref.OpenReadAsync()?).await?;
            log::debug!("[Cover Fetcher] 成功获取到封面流 (IRandomAccessStreamWithContentType)。");

            let stream_size = stream.Size()?;

            if stream_size == 0 {
                log::warn!("[Cover Fetcher] 未能获取到封面（流大小为0）。");
                return Ok(None);
            }
            if stream_size > MAX_COVER_SIZE_BYTES as u64 {
                log::warn!(
                    "[Cover Fetcher] 封面数据 ({stream_size} 字节) 超出最大限制 ({MAX_COVER_SIZE_BYTES} 字节)，已丢弃。"
                );
                return Ok(None);
            }

            let buffer = Buffer::Create(stream_size as u32)?;

            let read_operation = stream.ReadAsync(&buffer, buffer.Capacity()?, InputStreamOptions::None)?;
            let bytes_buffer = run_winrt_op_with_timeout(read_operation).await?;

            let reader = DataReader::FromBuffer(&bytes_buffer)?;
            let mut bytes = vec![0u8; bytes_buffer.Length()? as usize];
            reader.ReadBytes(&mut bytes)?;
            Ok(Some(bytes))
        } => res
    };

    match &result {
        Ok(Some(bytes)) => {
            log::debug!(
                "[Cover Fetcher] 获取到 {} 字节的封面数据。总耗时: {:?}",
                bytes.len(),
                start_time.elapsed()
            );
        }
        Ok(None) => {
            log::debug!(
                "[Cover Fetcher] 任务成功完成，但无有效封面数据。总耗时: {:?}",
                start_time.elapsed()
            );
        }
        Err(e) => {
            log::warn!(
                "[Cover Fetcher] 任务失败: {e:?}, 总耗时: {:?}",
                start_time.elapsed()
            );
        }
    }

    result
}

/// 一个独立的任务，用于平滑地调整指定进程的音量
async fn volume_easing_task(
    task_id: u64,
    target_vol: f32,
    session_id: String,
    connector_tx: TokioSender<InternalUpdate>,
    cancel_token: CancellationToken,
) {
    log::debug!(
        "[音量缓动任务][ID:{task_id}] 启动。目标音量: {target_vol:.2}，会话: '{session_id}'"
    );

    if let Ok((initial_vol, _)) = volume_control::get_volume_for_identifier(&session_id) {
        if (target_vol - initial_vol).abs() < VOLUME_EASING_THRESHOLD {
            let _ = volume_control::set_volume_for_identifier(&session_id, Some(target_vol), None);
            return;
        }

        log::trace!("[音量缓动任务][ID:{task_id}] 初始音量: {initial_vol:.2}");
        let animation_duration_ms = VOLUME_EASING_DURATION_MS;
        let steps = VOLUME_EASING_STEPS;
        let step_duration =
            TokioDuration::from_millis((animation_duration_ms / steps as f32) as u64);

        for s in 0..=steps {
            tokio::select! {
                biased;
                () = cancel_token.cancelled() => {
                    log::debug!("[音量缓动任务][ID:{task_id}] 任务被取消。");
                    break;
                }
                () = tokio::time::sleep(step_duration) => {
                    let current_time = (s as f32 / steps as f32) * animation_duration_ms;
                    let change_in_vol = target_vol - initial_vol;
                    let current_vol = easer::functions::Quad::ease_out(
                        current_time,
                        initial_vol,
                        change_in_vol,
                        animation_duration_ms,
                    );

                    if volume_control::set_volume_for_identifier(&session_id, Some(current_vol), None)
                        .is_err()
                    {
                        log::warn!("[音量缓动任务][ID:{task_id}] 设置音量失败，任务中止。");
                        break;
                    }
                }
            }
        }

        if let Ok((final_vol, final_mute)) = volume_control::get_volume_for_identifier(&session_id)
        {
            log::debug!("[音量缓动任务][ID:{task_id}] 完成。最终音量: {final_vol:.2}");
            let _ = connector_tx
                .send(InternalUpdate::AudioSessionVolumeChanged {
                    session_id,
                    volume: final_vol,
                    is_muted: final_mute,
                })
                .await;
        }
    } else {
        log::warn!("[音量缓动任务][ID:{task_id}] 无法获取初始音量，任务中止。");
    }
}

/// 一个独立的任务，用于定期计算并发送估算的播放进度。
async fn progress_timer_task(
    player_state_arc: Arc<TokioMutex<SharedPlayerState>>,
    progress_signal_tx: tokio::sync::mpsc::Sender<()>,
    cancel_token: CancellationToken,
) {
    log::debug!("[Timer] 任务已启动。");
    let mut interval = tokio::time::interval(TokioDuration::from_millis(100));

    loop {
        tokio::select! {
            () = cancel_token.cancelled() => {
                break;
            }
            _ = interval.tick() => {
                let state_guard = player_state_arc.lock().await;

                if state_guard.is_playing
                    && !state_guard.is_waiting_for_initial_update
                    && progress_signal_tx.send(()).await.is_err()
                {
                    log::warn!("[Timer] 无法发送进度更新信号，主事件循环可能已关闭。任务退出。");
                    break;
                }
            }
        }
    }
    log::debug!("[Timer] 任务已结束。");
}

/// `SmtcRunner` 封装了 SMTC 事件循环的所有状态和逻辑。
struct SmtcRunner {
    /// 内部状态，如 `session_guard`, `text_converter` 等。
    state: SmtcState,
    /// 用于向 Worker 发送状态更新。
    connector_update_tx: TokioSender<InternalUpdate>,
    /// 共享的播放器状态。
    player_state_arc: Arc<TokioMutex<SharedPlayerState>>,
    /// 用于向外部广播当前播放信息。
    now_playing_tx: watch::Sender<NowPlayingInfo>,
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
                            &context.active_volume_easing_task.take()
                        {
                            old_token.cancel();
                            // 启动一个分离的任务来等待旧任务结束，避免阻塞主循环
                            tokio::task::spawn_local(async move {
                                let _ = old_task;
                            });
                        }
                        let cancel_token = CancellationToken::new();
                        let task_id = context
                            .next_easing_task_id
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let handle = tokio::task::spawn_local(volume_easing_task(
                            task_id,
                            level,
                            session_id_str,
                            context.connector_update_tx.clone(),
                            cancel_token.clone(),
                        ));
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

        let mut session_check_interval =
            tokio::time::interval(std::time::Duration::from_millis(250));

        loop {
            pump_pending_messages();

            tokio::select! {
                biased;

                () = tokio::time::sleep(std::time::Duration::from_millis(16)) => {
                    // 这个分支的目的就是为了唤醒 select!
                },

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

                _ = session_check_interval.tick() => {
                    self.handle_session_check_tick(&smtc_event_tx)?;
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
        log::debug!("[SmtcRunner] 收到外部命令: {command:?}");
        match command {
            InternalCommand::SetTextConversion(mode) => {
                if self.state.text_conversion_mode != mode {
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
                        let _ = smtc_event_tx
                            .try_send(SmtcEventSignal::MediaProperties(id_hstr.to_string_lossy()));
                    }
                }
            }
            InternalCommand::SelectSmtcSession(id) => {
                let new_target = if id.is_empty() { None } else { Some(id) };
                log::info!("[SmtcRunner] 切换目标会话 -> {new_target:?}");
                self.state.target_session_id = new_target;
                let _ = smtc_event_tx.try_send(SmtcEventSignal::Sessions);
            }
            InternalCommand::MediaControl(media_cmd) => {
                let Some(guard) = &self.state.session_guard else {
                    log::warn!(
                        "[SmtcRunner] 收到媒体控制命令 {media_cmd:?}，但没有活动的会话，已忽略。"
                    );
                    return Ok(());
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
            InternalCommand::RequestStateUpdate => {
                if !self.state.is_manager_ready {
                    log::warn!("[SmtcRunner] 收到状态更新请求，但 SMTC 管理器尚未就绪，已忽略。");
                    return Ok(());
                }
                log::info!("[SmtcRunner] 正在重新获取所有状态...");
                self.state.session_guard = None;
                self.handle_sessions_changed(smtc_event_tx).await?;
            }
            InternalCommand::SetProgressTimer(enabled) => {
                if enabled {
                    if self.state.active_progress_timer_task.is_none() {
                        log::info!("[SmtcRunner] 启用进度计时器。");
                        let cancel_token = CancellationToken::new();
                        let handle = tokio::task::spawn_local(progress_timer_task(
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
        }
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
            SmtcEventSignal::Sessions => {
                if self.state.manager_guard.is_some() {
                    self.handle_sessions_changed(smtc_event_tx).await?;
                } else {
                    log::warn!("[SmtcRunner] 收到 SessionsChanged 信号，但管理器尚未就绪。");
                }
            }
            SmtcEventSignal::MediaProperties(event_session_id) => {
                if let Some(guard) = &self.state.session_guard
                    && guard
                        .session
                        .SourceAppUserModelId()
                        .ok()
                        .map(|h| h.to_string_lossy())
                        == Some(event_session_id.clone())
                {
                    let s = &guard.session;
                    spawn_async_op(
                        s.TryGetMediaPropertiesAsync(),
                        async_result_tx,
                        move |res| AsyncTaskResult::MediaPropertiesReady(event_session_id, res),
                    );
                }
            }
            SmtcEventSignal::PlaybackInfo(event_session_id) => {
                let current_session_id = self
                    .state
                    .session_guard
                    .as_ref()
                    .and_then(|g| g.session.SourceAppUserModelId().ok())
                    .map(|h| h.to_string_lossy());

                if current_session_id != Some(event_session_id) {
                    return Ok(());
                }

                if let Some(guard) = &self.state.session_guard
                    && let Ok(info) = guard.session.GetPlaybackInfo()
                {
                    let mut state_guard = self.player_state_arc.lock().await;

                    let session_id_after_await = self
                        .state
                        .session_guard
                        .as_ref()
                        .and_then(|g| g.session.SourceAppUserModelId().ok())
                        .map(|h| h.to_string_lossy());

                    if current_session_id != session_id_after_await {
                        return Ok(());
                    }

                    state_guard.is_playing = info.PlaybackStatus()
                        == Ok(GlobalSystemMediaTransportControlsSessionPlaybackStatus::Playing);

                    state_guard.is_shuffle_active = info
                        .IsShuffleActive()
                        .and_then(|iref| iref.Value())
                        .unwrap_or(false);
                    state_guard.repeat_mode = info
                        .AutoRepeatMode()
                        .and_then(|iref| iref.Value())
                        .map(|rm| match rm {
                            MediaPlaybackAutoRepeatMode::None => RepeatMode::Off,
                            MediaPlaybackAutoRepeatMode::Track => RepeatMode::One,
                            MediaPlaybackAutoRepeatMode::List => RepeatMode::All,
                            _ => RepeatMode::Off,
                        })
                        .unwrap_or(RepeatMode::Off);

                    if let Ok(c) = info.Controls() {
                        state_guard.can_pause = c.IsPauseEnabled().unwrap_or(false);
                        state_guard.can_play = c.IsPlayEnabled().unwrap_or(false);
                        state_guard.can_skip_next = c.IsNextEnabled().unwrap_or(false);
                        state_guard.can_skip_previous = c.IsPreviousEnabled().unwrap_or(false);
                        state_guard.can_seek = c.IsPlaybackPositionEnabled().unwrap_or(false);
                        state_guard.can_change_shuffle = c.IsShuffleEnabled().unwrap_or(false);
                        state_guard.can_change_repeat = c.IsRepeatEnabled().unwrap_or(false);
                    }
                    send_now_playing_update(&state_guard, &self.now_playing_tx);
                }
            }
            SmtcEventSignal::TimelineProperties(event_session_id) => {
                let current_session_id = self
                    .state
                    .session_guard
                    .as_ref()
                    .and_then(|g| g.session.SourceAppUserModelId().ok())
                    .map(|h| h.to_string_lossy());

                if current_session_id != Some(event_session_id) {
                    return Ok(());
                }

                if let Some(guard) = &self.state.session_guard
                    && let Ok(props) = guard.session.GetTimelineProperties()
                {
                    let mut state_guard = self.player_state_arc.lock().await;

                    let session_id_after_await = self
                        .state
                        .session_guard
                        .as_ref()
                        .and_then(|g| g.session.SourceAppUserModelId().ok())
                        .map(|h| h.to_string_lossy());

                    if current_session_id != session_id_after_await {
                        return Ok(());
                    }

                    let new_pos_ms = props.Position().map_or(0, |d| (d.Duration / 10000) as u64);
                    let dur_ms = props.EndTime().map_or(0, |d| (d.Duration / 10000) as u64);

                    let estimated_current_pos_ms = state_guard.get_estimated_current_position_ms();
                    let is_seek = (new_pos_ms as i64 - estimated_current_pos_ms as i64).abs()
                        > SEEK_DETECTION_THRESHOLD_MS as i64;

                    if is_seek || new_pos_ms > state_guard.last_known_position_ms {
                        state_guard.last_known_position_ms = new_pos_ms;
                        state_guard.last_known_position_report_time = Some(Instant::now());
                        if dur_ms > 0 {
                            state_guard.song_duration_ms = dur_ms;
                        }
                        if state_guard.is_waiting_for_initial_update {
                            state_guard.is_waiting_for_initial_update = false;
                            log::debug!("[SmtcRunner] 收到第一次 SMTC 更新，已清除等待标志。");
                        }
                        send_now_playing_update(&state_guard, &self.now_playing_tx);
                    }
                }
            }
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
                self.on_media_control_completed(cmd, res)
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

            let mut player_state = self.player_state_arc.lock().await;

            let is_new_track = player_state.title != title || player_state.artist != artist;

            if is_new_track {
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

            if is_new_track && let Ok(thumb_ref) = props.Thumbnail() {
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
                    let result = fetch_cover_data_task(thumb_ref, token_for_task).await;
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

            send_now_playing_update(&player_state, &self.now_playing_tx);
        } else if let Err(e) = result {
            log::warn!("[SmtcRunner] 获取媒体属性失败: {e:?}");
        }
        Ok(())
    }

    fn on_media_control_completed(
        &mut self,
        cmd: SmtcControlCommand,
        res: WinResult<bool>,
    ) -> WinResult<()> {
        match res {
            Ok(true) => log::debug!("[SmtcRunner] 媒体控制指令 {cmd:?} 成功执行。"),
            Ok(false) => log::warn!("[SmtcRunner] 媒体控制指令 {cmd:?} 执行失败 (返回 false)。"),
            Err(e) => log::warn!("[SmtcRunner] 媒体控制指令 {cmd:?} 调用失败: {e:?}"),
        }
        Ok(())
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
                    drop(player_state);
                    let player_state = self.player_state_arc.lock().await;
                    send_now_playing_update(&player_state, &self.now_playing_tx);
                }
            }
            Ok(None) => {
                let mut player_state = self.player_state_arc.lock().await;
                if player_state.cover_data.is_some() {
                    log::debug!("[State Update] 清空封面数据。");
                    player_state.cover_data = None;
                    player_state.cover_data_hash = None;
                    drop(player_state);
                    let player_state = self.player_state_arc.lock().await;
                    send_now_playing_update(&player_state, &self.now_playing_tx);
                }
            }
            Err(e) => {
                log::warn!("[SmtcRunner] 获取封面失败: {e:?}，正在重置会话。");
                {
                    let mut player_state = self.player_state_arc.lock().await;
                    player_state.cover_data = None;
                    player_state.cover_data_hash = None;
                }
                let player_state = self.player_state_arc.lock().await;
                send_now_playing_update(&player_state, &self.now_playing_tx);

                self.state.session_guard = None;

                if let Err(e) = smtc_event_tx.try_send(SmtcEventSignal::Sessions) {
                    log::error!("[SmtcRunner] 发送会话重置信号失败: {e:?}");
                }
            }
        }
        Ok(())
    }

    async fn handle_progress_update_signal(&mut self) {
        if let Some(guard) = &self.state.session_guard
            && let Ok(info) = guard.session.GetPlaybackInfo()
        {
            let is_playing_now = info.PlaybackStatus()
                == Ok(GlobalSystemMediaTransportControlsSessionPlaybackStatus::Playing);

            let mut player_state = self.player_state_arc.lock().await;
            player_state.is_playing = is_playing_now;
        }
        let player_state = self.player_state_arc.lock().await;
        send_now_playing_update(&player_state, &self.now_playing_tx);
    }

    fn handle_session_check_tick(
        &mut self,
        smtc_event_tx: &TokioSender<SmtcEventSignal>,
    ) -> WinResult<()> {
        if let Some(guard) = &self.state.manager_guard
            && self.state.target_session_id.is_none()
            && let Ok(current_session) = guard.manager.GetCurrentSession()
        {
            let new_session_id = current_session
                .SourceAppUserModelId()
                .ok()
                .map(|h| h.to_string_lossy());

            let monitored_session_id = self
                .state
                .session_guard
                .as_ref()
                .and_then(|g| g.session.SourceAppUserModelId().ok())
                .map(|h| h.to_string_lossy());

            if new_session_id != monitored_session_id {
                log::debug!(
                    "[会话轮询] 系统默认会话已更改 (从 {:?} 到 {:?})，正在刷新。",
                    monitored_session_id.as_deref().unwrap_or("无"),
                    new_session_id.as_deref().unwrap_or("无")
                );

                let _ = smtc_event_tx.try_send(SmtcEventSignal::Sessions);
            }
        }
        Ok(())
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
                manager.GetCurrentSession().map(Some)
            }
        } else {
            manager.GetCurrentSession().map(Some)
        }
    }

    async fn update_monitoring_session(
        &mut self,
        new_session_to_monitor: Option<MediaSession>,
        smtc_event_tx: &TokioSender<SmtcEventSignal>,
    ) -> WinResult<()> {
        let new_session_id = new_session_to_monitor
            .as_ref()
            .and_then(|s| s.SourceAppUserModelId().ok())
            .map(|h| h.to_string_lossy());

        let current_session_id = self
            .state
            .session_guard
            .as_ref()
            .and_then(|g| g.session.SourceAppUserModelId().ok())
            .map(|h| h.to_string_lossy());

        if new_session_id == current_session_id {
            return Ok(());
        }

        log::info!(
            "[会话处理器] 检测到会话切换: 从 {:?} -> 到 {:?}",
            current_session_id.as_deref().unwrap_or("无"),
            new_session_id.as_deref().unwrap_or("无")
        );

        self.state.session_guard = None;

        {
            let mut player_state = self.player_state_arc.lock().await;
            player_state.reset_to_empty();
        }
        let player_state = self.player_state_arc.lock().await;
        send_now_playing_update(&player_state, &self.now_playing_tx);

        if let Some(new_s) = new_session_to_monitor {
            let new_guard = MonitoredSessionGuard::new(new_s.clone(), smtc_event_tx)?;
            self.state.session_guard = Some(new_guard);

            log::info!("[会话处理器] 会话切换完成，正在获取初始状态。");
            if let Ok(id_hstr) = new_s.SourceAppUserModelId() {
                let new_session_id_str = id_hstr.to_string_lossy();
                let _ = smtc_event_tx
                    .try_send(SmtcEventSignal::MediaProperties(new_session_id_str.clone()));
                let _ = smtc_event_tx
                    .try_send(SmtcEventSignal::PlaybackInfo(new_session_id_str.clone()));
                let _ =
                    smtc_event_tx.try_send(SmtcEventSignal::TimelineProperties(new_session_id_str));
            }
        } else {
            log::info!("[会话处理器] 没有可用的媒体会话，已重置状态。");
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

fn pump_pending_messages() {
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
    now_playing_tx: watch::Sender<NowPlayingInfo>,
    diagnostics_tx: TokioSender<DiagnosticInfo>,
) -> Result<()> {
    let mut runner = SmtcRunner {
        state: SmtcState::new(),
        connector_update_tx,
        player_state_arc,
        now_playing_tx,
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
