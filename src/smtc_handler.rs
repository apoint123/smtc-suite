use std::{future::IntoFuture, sync::Arc, time::Instant};

use easer::functions::Easing;
use ferrous_opencc::OpenCC;
use tokio::{
    sync::{
        Mutex as TokioMutex,
        mpsc::{Receiver as TokioReceiver, Sender as TokioSender, channel as tokio_channel},
        watch,
    },
    task::{JoinHandle, LocalSet},
    time::{Duration as TokioDuration, timeout as tokio_timeout},
};
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
    Win32::{
        System::{
            Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize},
            Threading::GetCurrentThreadId,
        },
        UI::WindowsAndMessaging::{DispatchMessageW, GetMessageW, MSG, TranslateMessage, WM_QUIT},
    },
    core::{Error as WinError, HSTRING, Result as WinResult},
};
use windows_future::IAsyncOperation;

use crate::{
    api::{NowPlayingInfo, RepeatMode, SmtcControlCommand, SmtcSessionInfo, TextConversionMode},
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
        let tx_media = smtc_event_tx.clone();
        let tx_playback = smtc_event_tx.clone();
        let tx_timeline = smtc_event_tx.clone();

        let media_token =
            session.MediaPropertiesChanged(&TypedEventHandler::new(move |_, _| {
                let _ = tx_media.try_send(SmtcEventSignal::MediaProperties);
                Ok(())
            }))?;

        let playback_token =
            session.PlaybackInfoChanged(&TypedEventHandler::new(move |_, _| {
                let _ = tx_playback.try_send(SmtcEventSignal::PlaybackInfo);
                Ok(())
            }))?;

        let timeline_token =
            session.TimelinePropertiesChanged(&TypedEventHandler::new(move |_, _| {
                let _ = tx_timeline.try_send(SmtcEventSignal::TimelineProperties);
                Ok(())
            }))?;
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
/// 用于防止 WinRT 的异步调用无限期阻塞。
const SMTC_ASYNC_OPERATION_TIMEOUT: TokioDuration = TokioDuration::from_secs(5);

/// Windows API 操作被中止时返回的 HRESULT 错误码 (E_ABORT)。
/// 用于在超时发生时手动构造一个错误。
const E_ABORT_HRESULT: windows::core::HRESULT = windows::core::HRESULT(0x80004004_u32 as i32);

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

/// 计算封面图片数据的哈希值 (u64)，用于高效地检测封面图片是否发生变化。
pub fn calculate_cover_hash(data: &[u8]) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    data.hash(&mut hasher);
    hasher.finish()
}

/// SMTC 内部事件信号，用于在 WinRT 事件处理器和主事件循环之间传递具体事件类型。
///
/// 由于 WinRT 的事件回调是异步触发的，我们不能在回调中直接处理复杂逻辑。
/// 相反，回调仅发送一个简单的信号，由主循环的 `select!` 统一处理。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SmtcEventSignal {
    /// 媒体属性（标题、艺术家等）已更改。
    MediaProperties,
    /// 播放信息（播放/暂停状态、可用控件）已更改。
    PlaybackInfo,
    /// 时间线属性（播放位置、总时长）已更改。
    TimelineProperties,
    /// 系统中的媒体会话列表已更改。
    Sessions,
}

/// 封装了所有可能从后台异步任务返回的结果。
///
/// 主循环通过一个通道接收此枚举，以统一处理所有异步操作的完成或失败。
#[derive(Debug)]
enum AsyncTaskResult {
    /// `MediaSessionManager::RequestAsync` 的结果。
    ManagerReady(WinResult<MediaSessionManager>),
    /// `TryGetMediaPropertiesAsync` 的结果。
    MediaPropertiesReady(WinResult<GlobalSystemMediaTransportControlsSessionMediaProperties>),
    /// 媒体控制命令（如播放、暂停）的异步执行结果。
    MediaControlCompleted(SmtcControlCommand, WinResult<bool>),
    /// 封面数据获取任务的结果。
    CoverDataReady(WinResult<Option<Vec<u8>>>),
}

/// 封装了 `run_smtc_listener` 主事件循环中所有可变的状态。
struct SmtcState {
    /// RAII Guard，管理当前被监听的会话及其事件处理器。
    session_guard: Option<MonitoredSessionGuard>,
    /// RAII Guard, 用于管理 SMTC 会话管理器及其 SessionsChanged 事件。
    manager_guard: Option<ManagerEventGuard>,
    /// 用户通过 `SelectSession` 命令指定的目标会话 ID。
    target_session_id: Option<String>,
    /// 当前的文本转换模式。
    text_conversion_mode: TextConversionMode,
    /// 根据当前模式创建的 OpenCC 转换器实例。
    text_converter: Option<OpenCC>,
    /// 当前活动的音量缓动任务的句柄。
    active_volume_easing_task: Option<JoinHandle<()>>,
    /// 当前活动的封面获取任务的句柄。
    active_cover_fetch_task: Option<JoinHandle<()>>,
    /// 主动进度更新计时器任务的句柄。
    active_progress_timer_task: Option<JoinHandle<()>>,
    /// 用于为音量缓动任务生成唯一 ID。
    next_easing_task_id: Arc<std::sync::atomic::AtomicU64>,
}

impl SmtcState {
    /// 创建一个新的 SmtcState 实例，并初始化所有字段。
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
            next_easing_task_id: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }
}

/// 一个通用的辅助函数，用于将返回 `IAsyncOperation` 的 WinRT API 调用派发到 Tokio 的本地任务中执行。
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
            let mapped_result = match result {
                Ok(res) => result_mapper(res),
                Err(_) => {
                    log::warn!(
                        "[异步操作] WinRT 异步操作超时 (>{SMTC_ASYNC_OPERATION_TIMEOUT:?})。"
                    );
                    result_mapper(Err(WinError::from(E_ABORT_HRESULT)))
                }
            };

            if let Err(e) = tx.send(mapped_result).await {
                log::warn!("[异步操作] 无法将结果发送回主循环: {}", e);
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

async fn send_now_playing_update(
    player_state_arc: &Arc<TokioMutex<SharedPlayerState>>,
    now_playing_tx: &watch::Sender<NowPlayingInfo>,
) {
    let latest_info = {
        let state_guard = player_state_arc.lock().await;
        NowPlayingInfo::from(&*state_guard)
    };

    if now_playing_tx.send(latest_info).is_err() {
        log::warn!("[SMTC] 状态广播失败，所有接收者可能已关闭");
    }
}

/// 从 SMTC 会话中异步获取封面图片数据。
async fn fetch_cover_data_task(
    thumb_ref: IRandomAccessStreamReference,
) -> WinResult<Option<Vec<u8>>> {
    let start_time = Instant::now();
    log::debug!("[Cover Fetcher] 正在获取封面数据...");
    let result = async {
        let open_op = thumb_ref.OpenReadAsync()?;
        let stream = tokio::select! {
            biased;
            res = open_op.clone().into_future() => {
                res?
            },
            _ = tokio::time::sleep(SMTC_ASYNC_OPERATION_TIMEOUT) => {
                log::warn!("[Cover Fetcher] 打开封面流超时 (>{SMTC_ASYNC_OPERATION_TIMEOUT:?})。");
                if let Err(e) = open_op.Cancel() {
                    log::warn!("[Cover Fetcher] 取消 OpenReadAsync 操作失败: {e:?}");
                } else {
                    log::debug!("[Cover Fetcher] 取消 OpenReadAsync 操作成功。");
                }
                return Err(WinError::from(E_ABORT_HRESULT));
            }
        };
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
        let read_op = stream.ReadAsync(&buffer, buffer.Capacity()?, InputStreamOptions::None)?;
        let bytes_buffer = tokio::select! {
            biased;
            res = read_op.clone().into_future() => {
                res?
            },
            _ = tokio::time::sleep(SMTC_ASYNC_OPERATION_TIMEOUT) => {
                log::warn!("[Cover Fetcher] 读取封面数据超时 (>{SMTC_ASYNC_OPERATION_TIMEOUT:?})。");
                if let Err(e) = read_op.Cancel() {
                    log::warn!("[Cover Fetcher] 取消 ReadAsync 操作失败: {e:?}");
                } else {
                    log::debug!("[Cover Fetcher] 取消 ReadAsync 操作成功。");
                }
                return Err(WinError::from(E_ABORT_HRESULT));
            }
        };
        let reader = DataReader::FromBuffer(&bytes_buffer)?;
        let mut bytes = vec![0u8; bytes_buffer.Length()? as usize];
        reader.ReadBytes(&mut bytes)?;
        Ok(Some(bytes))
    }
    .await;

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
            tokio::time::sleep(step_duration).await;
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
) {
    log::debug!("[Timer] 任务已启动。");
    let mut interval = tokio::time::interval(TokioDuration::from_millis(100));

    loop {
        interval.tick().await;

        let state_guard = player_state_arc.lock().await;

        if state_guard.is_playing
            && !state_guard.is_waiting_for_initial_update
            && progress_signal_tx.send(()).await.is_err()
        {
            log::warn!("[Timer] 无法发送进度更新信号，主事件循环可能已关闭。任务退出。");
            break;
        }
    }
    log::debug!("[Timer] 任务已结束。");
}

/// SmtcRunner 封装了 SMTC 事件循环的所有状态和逻辑。
struct SmtcRunner {
    /// 内部状态，如 session_guard, text_converter 等。
    state: SmtcState,
    /// 用于向 Worker 发送状态更新。
    connector_update_tx: TokioSender<InternalUpdate>,
    /// 共享的播放器状态。
    player_state_arc: Arc<TokioMutex<SharedPlayerState>>,
    /// 用于向外部广播当前播放信息。
    now_playing_tx: watch::Sender<NowPlayingInfo>,
    control_rx: TokioReceiver<InternalCommand>,
    shutdown_rx: TokioReceiver<()>,
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
            tokio::select! {
                biased;

                _ = self.shutdown_rx.recv() => {
                    log::info!("[SmtcRunner] 收到关闭信号，准备退出...");
                    break Ok(());
                },

                Some(command) = self.control_rx.recv() => {
                    self.handle_internal_command(command, &smtc_event_tx, &async_result_tx, &progress_signal_tx).await?;
                },

                _ = session_check_interval.tick() => {
                    self.handle_session_check_tick(&smtc_event_tx).await?;
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

                Some(_) = progress_signal_rx.recv() => {
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

                    let config_name = match mode {
                        TextConversionMode::Off => None,
                        TextConversionMode::TraditionalToSimplified => Some("t2s.json"),
                        TextConversionMode::SimplifiedToTraditional => Some("s2t.json"),
                        TextConversionMode::SimplifiedToTaiwan => Some("s2tw.json"),
                        TextConversionMode::TaiwanToSimplified => Some("tw2s.json"),
                        TextConversionMode::SimplifiedToHongKong => Some("s2hk.json"),
                        TextConversionMode::HongKongToSimplified => Some("hk2s.json"),
                    };

                    self.state.text_converter =
                        config_name.and_then(|name| match OpenCC::from_config_name(name) {
                            Ok(converter) => Some(converter),
                            Err(e) => {
                                log::error!("[SmtcRunner] 加载 OpenCC 配置 '{name}' 失败: {e}");
                                None
                            }
                        });

                    let _ = smtc_event_tx.try_send(SmtcEventSignal::MediaProperties);
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
                    return Ok(());
                };
                let session = &guard.session;
                log::debug!("[SmtcRunner] 正在执行命令: {media_cmd:?}");
                match media_cmd {
                    SmtcControlCommand::SetVolume(level) => {
                        if let Ok(id_hstr) = session.SourceAppUserModelId() {
                            let session_id_str = hstring_to_string(&id_hstr);
                            if !session_id_str.is_empty() {
                                if let Some(old_task) = self.state.active_volume_easing_task.take()
                                {
                                    old_task.abort();
                                }
                                let task_id = self
                                    .state
                                    .next_easing_task_id
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                self.state.active_volume_easing_task =
                                    Some(tokio::task::spawn_local(volume_easing_task(
                                        task_id,
                                        level,
                                        session_id_str,
                                        self.connector_update_tx.clone(),
                                    )));
                            }
                        }
                    }
                    SmtcControlCommand::SetShuffle(is_active) => {
                        let async_op = session.TryChangeShuffleActiveAsync(is_active);
                        spawn_async_op(async_op, async_result_tx, move |res| {
                            AsyncTaskResult::MediaControlCompleted(media_cmd, res)
                        });
                    }
                    SmtcControlCommand::SetRepeatMode(repeat_mode) => {
                        let win_repeat_mode = match repeat_mode {
                            RepeatMode::Off => MediaPlaybackAutoRepeatMode::None,
                            RepeatMode::One => MediaPlaybackAutoRepeatMode::Track,
                            RepeatMode::All => MediaPlaybackAutoRepeatMode::List,
                        };
                        let async_op = session.TryChangeAutoRepeatModeAsync(win_repeat_mode);
                        spawn_async_op(async_op, async_result_tx, move |res| {
                            AsyncTaskResult::MediaControlCompleted(media_cmd, res)
                        });
                    }
                    other_cmd => {
                        let async_op = match other_cmd {
                            SmtcControlCommand::Play => session.TryPlayAsync(),
                            SmtcControlCommand::Pause => session.TryPauseAsync(),
                            SmtcControlCommand::SkipNext => session.TrySkipNextAsync(),
                            SmtcControlCommand::SkipPrevious => session.TrySkipPreviousAsync(),
                            SmtcControlCommand::SeekTo(pos) => {
                                session.TryChangePlaybackPositionAsync(pos as i64 * 10000)
                            }
                            _ => unreachable!(),
                        };
                        spawn_async_op(async_op, async_result_tx, move |res| {
                            AsyncTaskResult::MediaControlCompleted(other_cmd, res)
                        });
                    }
                }
            }
            InternalCommand::RequestStateUpdate => {
                log::info!("[SmtcRunner] 正在重新获取所有状态...");
                self.state.session_guard = None;
                self.handle_sessions_changed(smtc_event_tx).await?;
            }
            InternalCommand::SetProgressTimer(enabled) => {
                if enabled {
                    if self.state.active_progress_timer_task.is_none() {
                        log::info!("[SmtcRunner] 启用进度计时器。");
                        let handle = tokio::task::spawn_local(progress_timer_task(
                            self.player_state_arc.clone(),
                            progress_signal_tx.clone(),
                        ));
                        self.state.active_progress_timer_task = Some(handle);
                    }
                } else if let Some(task) = self.state.active_progress_timer_task.take() {
                    log::info!("[SmtcRunner] 禁用进度计时器。");
                    task.abort();
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
            SmtcEventSignal::MediaProperties => {
                if let Some(guard) = &self.state.session_guard {
                    let s = &guard.session;
                    spawn_async_op(
                        s.TryGetMediaPropertiesAsync(),
                        async_result_tx,
                        AsyncTaskResult::MediaPropertiesReady,
                    );
                }
            }
            SmtcEventSignal::PlaybackInfo => {
                if let Some(guard) = &self.state.session_guard {
                    if let Ok(info) = guard.session.GetPlaybackInfo() {
                        let is_playing_now = info.PlaybackStatus()
                            == Ok(GlobalSystemMediaTransportControlsSessionPlaybackStatus::Playing);

                        let should_update_state;
                        {
                            let player_state = self.player_state_arc.lock().await;
                            should_update_state = player_state.is_playing != is_playing_now;
                        }

                        if should_update_state {
                            log::trace!(
                                "[State Update] 播放状态发生改变 -> {}",
                                if is_playing_now { "播放" } else { "暂停" }
                            );
                            let current_pos_ms = guard
                                .session
                                .GetTimelineProperties()
                                .ok()
                                .and_then(|props| props.Position().ok())
                                .map(|d| (d.Duration / 10000) as u64);

                            {
                                let mut state_guard = self.player_state_arc.lock().await;
                                state_guard.is_playing = is_playing_now;
                                state_guard.last_known_position_report_time = Some(Instant::now());

                                if let Some(pos) = current_pos_ms {
                                    state_guard.last_known_position_ms = pos;
                                }
                            }
                        }

                        {
                            let mut state_guard = self.player_state_arc.lock().await;
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
                                state_guard.can_skip_previous =
                                    c.IsPreviousEnabled().unwrap_or(false);
                                state_guard.can_seek =
                                    c.IsPlaybackPositionEnabled().unwrap_or(false);
                                state_guard.can_change_shuffle =
                                    c.IsShuffleEnabled().unwrap_or(false);
                                state_guard.can_change_repeat =
                                    c.IsRepeatEnabled().unwrap_or(false);
                            }
                        }
                        send_now_playing_update(&self.player_state_arc, &self.now_playing_tx).await;
                    }
                }
            }
            SmtcEventSignal::TimelineProperties => {
                if let Some(guard) = &self.state.session_guard {
                    match guard.session.GetTimelineProperties() {
                        Ok(props) => {
                            let new_pos_ms =
                                props.Position().map_or(0, |d| (d.Duration / 10000) as u64);
                            let dur_ms = props.EndTime().map_or(0, |d| (d.Duration / 10000) as u64);

                            let mut state_guard = self.player_state_arc.lock().await;

                            let estimated_current_pos_ms =
                                state_guard.get_estimated_current_position_ms();

                            let is_seek = (new_pos_ms as i64 - estimated_current_pos_ms as i64)
                                .abs()
                                > SEEK_DETECTION_THRESHOLD_MS as i64;

                            if is_seek || new_pos_ms > state_guard.last_known_position_ms {
                                state_guard.last_known_position_ms = new_pos_ms;
                                state_guard.last_known_position_report_time = Some(Instant::now());

                                if dur_ms > 0 {
                                    state_guard.song_duration_ms = dur_ms;
                                }

                                if state_guard.is_waiting_for_initial_update {
                                    state_guard.is_waiting_for_initial_update = false;
                                    log::debug!(
                                        "[SmtcRunner] 收到第一次 SMTC 更新，已清除等待标志。"
                                    );
                                }

                                drop(state_guard);
                                send_now_playing_update(
                                    &self.player_state_arc,
                                    &self.now_playing_tx,
                                )
                                .await;
                            }
                        }
                        Err(e) => log::warn!("[SmtcRunner] 获取 TimelineProperties 失败: {e:?}"),
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
            AsyncTaskResult::ManagerReady(Ok(mgr)) => {
                log::trace!("[SmtcRunner] SMTC 管理器已就绪。");
                let guard = ManagerEventGuard::new(mgr, smtc_event_tx)?;
                self.state.manager_guard = Some(guard);
                let _ = smtc_event_tx.try_send(SmtcEventSignal::Sessions);
            }
            AsyncTaskResult::ManagerReady(Err(e)) => {
                log::error!("[SmtcRunner] 初始化 SMTC 管理器失败: {e:?}。");
                return Err(e);
            }
            AsyncTaskResult::MediaPropertiesReady(Ok(props)) => {
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
                let is_placeholder_title = IGNORED_TITLES.iter().any(|&ignored| title == ignored);

                if !is_placeholder_title {
                    let should_fetch_cover;
                    {
                        let mut player_state = self.player_state_arc.lock().await;
                        log::info!(
                            "[SmtcRunner] 接收到新曲目信息: '{}' - '{}'",
                            &artist,
                            &title
                        );

                        let is_new_track =
                            player_state.title != title || player_state.artist != artist;
                        if is_new_track {
                            player_state.cover_data = None;
                            player_state.cover_data_hash = None;
                            player_state.is_waiting_for_initial_update = true;
                            should_fetch_cover = true;
                        } else {
                            should_fetch_cover = false;
                        }

                        player_state.title = title;
                        player_state.artist = artist;
                        player_state.album = album;
                    }

                    if should_fetch_cover {
                        if let Ok(thumb_ref) = props.Thumbnail() {
                            if let Some(old_task) = self.state.active_cover_fetch_task.take() {
                                old_task.abort();
                                log::trace!("[SmtcRunner] 已取消旧的封面获取任务");
                            }

                            let async_result_tx_clone = async_result_tx.clone();
                            let cover_task = tokio::task::spawn_local(async move {
                                let result = fetch_cover_data_task(thumb_ref).await;
                                if async_result_tx_clone
                                    .send(AsyncTaskResult::CoverDataReady(result))
                                    .await
                                    .is_err()
                                {
                                    log::warn!(
                                        "[Cover Fetcher] 无法将封面结果发送回主循环，通道已关闭。"
                                    );
                                }
                            });

                            self.state.active_cover_fetch_task = Some(cover_task);
                        }
                    }

                    send_now_playing_update(&self.player_state_arc, &self.now_playing_tx).await;
                }
            }
            AsyncTaskResult::MediaPropertiesReady(Err(e)) => {
                log::warn!("[SmtcRunner] 获取媒体属性失败: {e:?}");
            }
            AsyncTaskResult::MediaControlCompleted(cmd, res) => match res {
                Ok(true) => log::debug!("[SmtcRunner] 媒体控制指令 {cmd:?} 成功执行。"),
                Ok(false) => {
                    log::warn!("[SmtcRunner] 媒体控制指令 {cmd:?} 执行失败 (返回 false)。")
                }
                Err(e) => log::warn!("[SmtcRunner] 媒体控制指令 {cmd:?} 调用失败: {e:?}"),
            },
            AsyncTaskResult::CoverDataReady(result) => match result {
                Ok(Some(bytes)) => {
                    let new_hash = calculate_cover_hash(&bytes);
                    let mut player_state = self.player_state_arc.lock().await;
                    if player_state.cover_data_hash != Some(new_hash) {
                        log::debug!("[State Update] 封面已更新 (大小: {} 字节)。", bytes.len());
                        player_state.cover_data = Some(bytes);
                        player_state.cover_data_hash = Some(new_hash);
                        drop(player_state);
                        send_now_playing_update(&self.player_state_arc, &self.now_playing_tx).await;
                    }
                }
                Ok(None) => {
                    let mut player_state = self.player_state_arc.lock().await;
                    if player_state.cover_data.is_some() {
                        log::debug!("[State Update] 清空封面数据。");
                        player_state.cover_data = None;
                        player_state.cover_data_hash = None;
                        drop(player_state);
                        send_now_playing_update(&self.player_state_arc, &self.now_playing_tx).await;
                    }
                }
                Err(e) => {
                    log::warn!("[SmtcRunner] 获取封面失败: {e:?}，正在重置会话。");
                    {
                        let mut player_state = self.player_state_arc.lock().await;
                        player_state.cover_data = None;
                        player_state.cover_data_hash = None;
                    }
                    send_now_playing_update(&self.player_state_arc, &self.now_playing_tx).await;

                    self.state.session_guard = None;

                    if let Err(e) = smtc_event_tx.try_send(SmtcEventSignal::Sessions) {
                        log::error!("[SmtcRunner] 发送会话重置信号失败: {e:?}");
                    }
                }
            },
        }
        Ok(())
    }

    async fn handle_progress_update_signal(&mut self) {
        if let Some(guard) = &self.state.session_guard {
            if let Ok(info) = guard.session.GetPlaybackInfo() {
                let is_playing_now = info.PlaybackStatus()
                    == Ok(GlobalSystemMediaTransportControlsSessionPlaybackStatus::Playing);

                let mut player_state = self.player_state_arc.lock().await;
                player_state.is_playing = is_playing_now;
            }
        }
        send_now_playing_update(&self.player_state_arc, &self.now_playing_tx).await;
    }

    async fn handle_session_check_tick(
        &mut self,
        smtc_event_tx: &TokioSender<SmtcEventSignal>,
    ) -> WinResult<()> {
        if let Some(guard) = &self.state.manager_guard {
            if self.state.target_session_id.is_none() {
                if let Ok(current_session) = guard.manager.GetCurrentSession() {
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
            }
        }
        Ok(())
    }

    async fn handle_sessions_changed(
        &mut self,
        smtc_event_tx: &TokioSender<SmtcEventSignal>,
    ) -> WinResult<()> {
        log::debug!("[会话处理器] 开始处理会话变更...");

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

        let new_session_to_monitor = if let Some(target_id) = self.state.target_session_id.as_ref()
        {
            if let Some((_, session)) = session_candidates
                .into_iter()
                .find(|(id, _)| id == target_id)
            {
                Some(session)
            } else {
                log::warn!("[会话处理器] 目标会话 '{target_id}' 已消失。");
                let _ = self
                    .connector_update_tx
                    .send(InternalUpdate::SelectedSmtcSessionVanished(
                        target_id.clone(),
                    ))
                    .await;
                self.state.target_session_id = None;
                manager.GetCurrentSession().ok()
            }
        } else {
            manager.GetCurrentSession().ok()
        };

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

        if new_session_id != current_session_id {
            log::info!(
                "[会话处理器] 检测到会话切换: 从 {:?} -> 到 {:?}",
                current_session_id.as_deref().unwrap_or("无"),
                new_session_id.as_deref().unwrap_or("无")
            );

            self.state.session_guard = None;

            if let Some(new_s) = new_session_to_monitor {
                let new_guard = MonitoredSessionGuard::new(new_s, smtc_event_tx)?;
                {
                    let mut player_state = self.player_state_arc.lock().await;
                    player_state.reset_to_empty();
                }
                send_now_playing_update(&self.player_state_arc, &self.now_playing_tx).await;
                self.state.session_guard = Some(new_guard);

                log::info!("[会话处理器] 会话切换完成，正在获取所有初始状态。");
                let _ = smtc_event_tx.try_send(SmtcEventSignal::MediaProperties);
                let _ = smtc_event_tx.try_send(SmtcEventSignal::PlaybackInfo);
                let _ = smtc_event_tx.try_send(SmtcEventSignal::TimelineProperties);
            } else {
                log::info!("[会话处理器] 没有可用的媒体会话，重置状态。");
                {
                    let mut player_state = self.player_state_arc.lock().await;
                    player_state.reset_to_empty();
                }
                send_now_playing_update(&self.player_state_arc, &self.now_playing_tx).await;
            }
        }
        Ok(())
    }
}

pub fn run_smtc_listener(
    connector_update_tx: TokioSender<InternalUpdate>,
    control_rx: TokioReceiver<InternalCommand>,
    player_state_arc: Arc<TokioMutex<SharedPlayerState>>,
    shutdown_rx: TokioReceiver<()>,
    now_playing_tx: watch::Sender<NowPlayingInfo>,
) -> Result<()> {
    log::info!("[SMTC Handler] 正在启动 SMTC 监听器");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let local_set = LocalSet::new();

    local_set.block_on(&rt, async move {
        struct ComGuard;
        impl ComGuard {
            fn new() -> WinResult<Self> {
                unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()? };
                Ok(Self)
            }
        }
        impl Drop for ComGuard {
            fn drop(&mut self) {
                unsafe { CoUninitialize() };
            }
        }

        if let Err(e) = ComGuard::new() {
            log::error!("[SMTC Handler] COM 初始化失败 (STA): {e}，监听器线程无法启动。");
            return;
        }

        let (thread_id_tx, thread_id_rx) = std::sync::mpsc::channel::<u32>();
        let message_pump_handle = tokio::task::spawn_blocking(move || {
            let thread_id = unsafe { GetCurrentThreadId() };
            if thread_id_tx.send(thread_id).is_err() {
                log::error!("[消息泵] 无法发送线程ID，启动失败！");
                return;
            }
            unsafe {
                let mut msg = MSG::default();
                while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
        });

        let pump_thread_id = match thread_id_rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(id) => id,
            Err(_) => {
                log::error!("[SMTC 主循环] 等待消息泵线程ID超时，启动失败！");
                return;
            }
        };
        log::debug!("[SMTC 主循环] 已成功获取消息泵线程ID: {pump_thread_id}");

        let mut runner = SmtcRunner {
            state: SmtcState::new(),
            connector_update_tx,
            player_state_arc,
            now_playing_tx,
            control_rx,
            shutdown_rx,
        };

        if let Err(e) = runner.run().await {
            log::error!("[SmtcRunner] 事件循环因错误退出: {e:?}");
        }

        let state = runner.state;
        if let Some(task) = state.active_volume_easing_task {
            task.abort();
        }
        if let Some(task) = state.active_cover_fetch_task {
            task.abort();
        }
        if let Some(task) = state.active_progress_timer_task {
            task.abort();
        }

        // 发送 WM_QUIT 消息来停止消息泵线程
        // SAFETY: PostThreadMessageW 是向特定线程发送消息的标准 Win32 API。
        // 我们在启动时获取了正确的线程ID `main_thread_id`，所以这是安全的。
        unsafe {
            use windows::Win32::Foundation::{LPARAM, WPARAM};
            use windows::Win32::UI::WindowsAndMessaging::PostThreadMessageW;

            if PostThreadMessageW(pump_thread_id, WM_QUIT, WPARAM(0), LPARAM(0)).is_err() {
                log::error!(
                    "[SMTC 清理] 发送 WM_QUIT 消息到消息泵线程 (ID:{pump_thread_id}) 失败。"
                );
            }
        }
        if let Err(e) = message_pump_handle.await {
            log::warn!("[SMTC 清理] 等待消息泵线程退出时出错: {e:?}");
        }
    });

    log::info!("[SMTC Handler] 监听器线程已完全退出。");
    Ok(())
}
