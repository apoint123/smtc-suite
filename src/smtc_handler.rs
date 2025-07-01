use std::{future::IntoFuture, sync::Arc, time::Instant};

use crossbeam_channel::{Receiver as CrossbeamReceiver, Sender as CrossbeamSender};
use easer::functions::Easing;
use ferrous_opencc::OpenCC;
use tokio::{
    sync::{
        Mutex as TokioMutex,
        mpsc::{Sender as TokioSender, channel as tokio_channel, error::TrySendError},
    },
    task::{JoinHandle, LocalSet},
    time::{Duration as TokioDuration, timeout as tokio_timeout},
};
use windows::{
    Foundation::TypedEventHandler,
    Media::Control::{
        GlobalSystemMediaTransportControlsSession as MediaSession,
        GlobalSystemMediaTransportControlsSessionManager as MediaSessionManager,
        GlobalSystemMediaTransportControlsSessionMediaProperties,
        GlobalSystemMediaTransportControlsSessionPlaybackStatus,
    },
    Media::MediaPlaybackAutoRepeatMode,
    Storage::Streams::{
        Buffer, DataReader, IRandomAccessStreamReference, IRandomAccessStreamWithContentType,
        InputStreamOptions,
    },
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

/// 定义一个状态更新函数的类型别名。
/// 这是一个闭包，它接收一个 `SharedPlayerState` 的可变引用，并对其进行修改。
type StateUpdateFn = Box<dyn FnOnce(&mut SharedPlayerState) + Send>;

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
}

/// 封装了 `run_smtc_listener` 主事件循环中所有可变的状态。
struct SmtcState {
    /// SMTC 会话管理器。
    manager: Option<MediaSessionManager>,
    /// `SessionsChanged` 事件的监听器 Token。
    manager_sessions_changed_token: Option<i64>,
    /// 当前正在监听的媒体会话。
    current_monitored_session: Option<MediaSession>,
    /// 当前会话的三个事件（属性、播放、时间线）的监听器 Token。
    current_listener_tokens: Option<(i64, i64, i64)>,
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
            manager: None,
            manager_sessions_changed_token: None,
            current_monitored_session: None,
            current_listener_tokens: None,
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

// --- 异步任务处理器 ---

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
                Ok(res) => result_mapper(res), // 异步操作成功完成或返回了其内部错误
                Err(_) => {
                    log::warn!(
                        "[异步操作] WinRT 异步操作超时 (>{SMTC_ASYNC_OPERATION_TIMEOUT:?})。"
                    );
                    result_mapper(Err(WinError::from(E_ABORT_HRESULT))) // 封装成超时错误
                }
            };

            match tx.try_send(mapped_result) {
                Ok(_) => {} // 发送成功
                Err(TrySendError::Full(_)) => {
                    log::warn!("[异步操作] 无法将结果发送回主循环，业务繁忙，通道已满。");
                }
                Err(TrySendError::Closed(_)) => {
                    log::warn!("[异步操作] 无法将结果发送回主循环，通道已关闭，任务将提前中止。");
                }
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

/// 从 SMTC 会话中异步获取封面图片数据。
///
/// 获取成功后，它会构建一个 `StateUpdateFn` 闭包，并通过通道发送给状态管理器 Actor 进行更新。
async fn fetch_cover_data_async(
    thumb_ref: IRandomAccessStreamReference,
    state_update_tx: TokioSender<StateUpdateFn>,
) {
    log::debug!("[Cover Fetcher] 开始异步获取封面数据...");

    // 异步块，用于安全地处理所有可能失败的 WinRT 调用。
    let cover_result: WinResult<Option<Vec<u8>>> = async {
        // 1. 异步打开数据流
        let stream_op: IAsyncOperation<IRandomAccessStreamWithContentType> =
            thumb_ref.OpenReadAsync()?;
        let stream = tokio_timeout(SMTC_ASYNC_OPERATION_TIMEOUT, stream_op.into_future())
            .await
            .map_err(|_| {
                log::warn!("[Cover Fetcher] 打开封面流超时。");
                WinError::from(E_ABORT_HRESULT)
            })??;

        // 2. 检查流是否为空
        if stream.Size()? == 0 {
            log::debug!("[Cover Fetcher] 媒体会话提供了空的封面流。");
            return Ok(None);
        }

        // 3. 创建缓冲区并异步读取所有数据
        let buffer = Buffer::Create(stream.Size()? as u32)?;
        let read_op = stream.ReadAsync(&buffer, buffer.Capacity()?, InputStreamOptions::None)?;

        let bytes_buffer = tokio_timeout(SMTC_ASYNC_OPERATION_TIMEOUT, read_op.into_future())
            .await
            .map_err(|_| {
                log::warn!("[Cover Fetcher] 读取封面数据超时。");
                WinError::from(E_ABORT_HRESULT)
            })??;

        // 4. 将缓冲区中的数据转换为 Vec<u8>
        let reader = DataReader::FromBuffer(&bytes_buffer)?;
        let mut bytes = vec![0u8; bytes_buffer.Length()? as usize];
        reader.ReadBytes(&mut bytes)?;
        Ok(Some(bytes))
    }
    .await;

    // 根据获取结果，构建一个状态更新闭包
    let update_closure: StateUpdateFn = Box::new(move |state| {
        match cover_result {
            Ok(Some(bytes)) => {
                // 检查数据大小是否超出限制
                if bytes.len() > MAX_COVER_SIZE_BYTES {
                    log::warn!(
                        "[Cover Fetcher] 获取到的封面数据 ({} 字节) 超出最大限制 ({} 字节)，已丢弃。",
                        bytes.len(),
                        MAX_COVER_SIZE_BYTES
                    );
                    // 即使丢弃，也清空状态以防万一
                    state.cover_data = None;
                    state.cover_data_hash = None;
                    return;
                }

                // 计算新封面的哈希值，只有当封面真正变化时才更新状态
                let new_hash = calculate_cover_hash(&bytes);
                if state.cover_data_hash != Some(new_hash) {
                    log::debug!("[State Update] 封面已更新 (大小: {} 字节)。", bytes.len());
                    state.cover_data_hash = Some(new_hash);
                    state.cover_data = Some(bytes);
                } else {
                    log::trace!("[State Update] 封面哈希值未变，无需更新。");
                }
            }
            Ok(None) => {
                // 如果获取到的是空流，则清空封面状态
                log::debug!("[State Update] 清空封面，因为获取到的流为空。");
                state.cover_data = None;
                state.cover_data_hash = None;
            }
            Err(e) => {
                // 如果在获取过程中发生任何错误，也清空封面状态
                log::warn!("[State Update] 因获取封面失败而清空封面: {e:?}");
                state.cover_data = None;
                state.cover_data_hash = None;
            }
        }
    });

    // 将这个状态更新闭包发送给状态管理器 Actor
    if let Err(e) = state_update_tx.try_send(update_closure) {
        match e {
            TrySendError::Full(_) => {
                log::warn!("[Cover Fetcher] 状态更新通道已满，本次封面更新可能被丢弃。");
            }
            TrySendError::Closed(_) => {
                log::warn!("[Cover Fetcher] 状态更新通道已关闭，无法发送封面数据。");
            }
        }
    }
}

/// 一个独立的任务，用于平滑地调整指定进程的音量
async fn volume_easing_task(
    task_id: u64,
    target_vol: f32,
    session_id: String,
    connector_tx: CrossbeamSender<InternalUpdate>,
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
            let _ = connector_tx.send(InternalUpdate::AudioSessionVolumeChanged {
                session_id,
                volume: final_vol,
                is_muted: final_mute,
            });
        }
    } else {
        log::warn!("[音量缓动任务][ID:{task_id}] 无法获取初始音量，任务中止。");
    }
}

/// 一个独立的任务，用于定期计算并发送估算的播放进度。
async fn progress_timer_task(
    player_state_arc: Arc<TokioMutex<SharedPlayerState>>,
    connector_update_tx: CrossbeamSender<InternalUpdate>,
) {
    log::debug!("[Timer] 任务已启动。");
    let mut interval = tokio::time::interval(TokioDuration::from_millis(100));

    loop {
        interval.tick().await;

        let state_guard = player_state_arc.lock().await;

        if state_guard.is_playing && !state_guard.is_waiting_for_initial_update {
            let progress_update_info = NowPlayingInfo {
                position_ms: Some(state_guard.get_estimated_current_position_ms()),
                position_report_time: state_guard.last_known_position_report_time,
                is_playing: Some(state_guard.is_playing),
                title: Some(state_guard.title.clone()),
                artist: Some(state_guard.artist.clone()),
                album_title: Some(state_guard.album.clone()),
                duration_ms: Some(state_guard.song_duration_ms),
                is_shuffle_active: Some(state_guard.is_shuffle_active),
                repeat_mode: Some(state_guard.repeat_mode),
                can_play: Some(state_guard.can_play),
                can_pause: Some(state_guard.can_pause),
                can_skip_next: Some(state_guard.can_skip_next),
                can_skip_previous: Some(state_guard.can_skip_previous),
                cover_data: None,
                cover_data_hash: None,
            };

            if connector_update_tx
                .send(InternalUpdate::NowPlayingTrackChanged(progress_update_info))
                .is_err()
            {
                log::warn!("[Timer] 无法发送进度更新，主连接器可能已关闭。任务退出。");
                break;
            }
        }
    }
    log::debug!("[Timer] 任务已结束。");
}

/// 处理 SMTC 会话列表发生变化的事件。
///
/// 这是 SMTC 监听器的核心逻辑之一。它负责：
/// 1. 获取所有可用的媒体会话列表并通知外部。
/// 2. 根据 `target_session_id`（用户选择）或默认规则（系统当前会话）来决定要监听哪个会话。
/// 3. 如果需要切换会话，它会注销旧会话的事件监听器，并为新会话注册新的监听器。
/// 4. 如果没有可用的会话，它会重置播放状态。
fn handle_sessions_changed(
    state: &mut SmtcState,
    connector_update_tx: &CrossbeamSender<InternalUpdate>,
    smtc_event_tx: &TokioSender<SmtcEventSignal>,
    state_update_tx: &TokioSender<StateUpdateFn>,
) -> WinResult<()> {
    log::debug!("[会话处理器] 开始处理会话变更...");

    // 从状态上下文中获取 SMTC 管理器，如果尚未初始化则直接返回。
    let manager = state
        .manager
        .as_ref()
        .ok_or_else(|| WinError::from(E_ABORT_HRESULT))?;

    // 1. 获取所有当前会话的信息
    let sessions_ivector = manager.GetSessions()?;
    let mut sessions_info_list = Vec::new();
    let mut session_candidates = Vec::new(); // 存储 (ID, MediaSession) 元组

    for s in sessions_ivector {
        if let Ok(id_hstr) = s.SourceAppUserModelId() {
            let id_str = hstring_to_string(&id_hstr);
            if id_str.is_empty() {
                log::trace!("[会话处理器] 发现一个没有有效ID的会话，已忽略。");
                continue;
            }
            log::trace!("[会话处理器] 发现会话: '{id_str}'");
            sessions_info_list.push(SmtcSessionInfo {
                source_app_user_model_id: id_str.clone(),
                session_id: id_str.clone(),
                display_name: crate::utils::get_display_name_from_smtc_id(&id_str),
            });
            session_candidates.push((id_str, s.clone()));
        }
    }
    // 将最新的会话列表发送出去
    if let Err(e) =
        connector_update_tx.send(InternalUpdate::SmtcSessionListChanged(sessions_info_list))
    {
        log::warn!("[会话处理器] 无法发送会话列表更新，主连接器可能已关闭: {e:?}");
    }

    // 2. 决定要监控哪个会话
    let new_session_to_monitor = if let Some(target_id) = state.target_session_id.as_ref() {
        log::debug!("[会话处理器] 正在寻找目标会话: '{target_id}'");
        if let Some((_, session)) = session_candidates
            .into_iter()
            .find(|(id, _)| id == target_id)
        {
            log::info!("[会话处理器] 已成功找到并选择目标会话: '{target_id}'");
            Some(session)
        } else {
            // 用户指定的目标会话消失了
            log::warn!("[会话处理器] 目标会话 '{target_id}' 已消失。");
            let _ = connector_update_tx.send(InternalUpdate::SelectedSmtcSessionVanished(
                target_id.clone(),
            ));
            state.target_session_id = None; // 清除目标，回到自动模式
            log::info!("[会话处理器] 已清除目标会话，回退到默认会话。");
            manager.GetCurrentSession().ok() // 尝试获取系统当前的会话作为备用
        }
    } else {
        log::debug!("[会话处理器] 处于自动模式，将使用默认会话。");
        manager.GetCurrentSession().ok() // 自动模式，获取系统当前会话
    };

    let new_session_id = new_session_to_monitor
        .as_ref()
        .and_then(|s| s.SourceAppUserModelId().ok())
        .map(|h| h.to_string_lossy());

    let current_session_id = state
        .current_monitored_session
        .as_ref()
        .and_then(|s| s.SourceAppUserModelId().ok())
        .map(|h| h.to_string_lossy());

    // 3. 检查是否需要切换会话
    if new_session_id != current_session_id {
        log::info!(
            "[会话处理器] 检测到会话切换: 从 {:?} -> 到 {:?}",
            current_session_id.as_deref().unwrap_or("无"),
            new_session_id.as_deref().unwrap_or("无")
        );

        // 3a. 注销旧会话的监听器
        if let Some(old_s) = state.current_monitored_session.take()
            && let Some(tokens) = state.current_listener_tokens.take()
        {
            log::debug!("[会话处理器] 正在注销旧会话的事件监听器...");
            let _ = old_s
                .RemoveMediaPropertiesChanged(tokens.0)
                .map_err(|e| log::warn!("注销 MediaPropertiesChanged 失败: {e}"));
            let _ = old_s
                .RemovePlaybackInfoChanged(tokens.1)
                .map_err(|e| log::warn!("注销 PlaybackInfoChanged 失败: {e}"));
            let _ = old_s
                .RemoveTimelinePropertiesChanged(tokens.2)
                .map_err(|e| log::warn!("注销 TimelinePropertiesChanged 失败: {e}"));
        }

        // 3b. 为新会话设置监听器
        if let Some(new_s) = new_session_to_monitor {
            log::debug!("[会话处理器] 正在为新会话注册事件监听器...");
            let tx_media = smtc_event_tx.clone();
            let tx_playback = smtc_event_tx.clone();
            let tx_timeline = smtc_event_tx.clone();

            let tokens = (
                new_s.MediaPropertiesChanged(&TypedEventHandler::new(move |_, _| {
                    log::trace!("[Event] MediaPropertiesChanged 信号触发");
                    let _ = tx_media.try_send(SmtcEventSignal::MediaProperties);
                    Ok(())
                }))?,
                new_s.PlaybackInfoChanged(&TypedEventHandler::new(move |_, _| {
                    log::trace!("[Event] PlaybackInfoChanged 信号触发");
                    let _ = tx_playback.try_send(SmtcEventSignal::PlaybackInfo);
                    Ok(())
                }))?,
                new_s.TimelinePropertiesChanged(&TypedEventHandler::new(move |_, _| {
                    log::trace!("[Event] TimelinePropertiesChanged 信号触发");
                    let _ = tx_timeline.try_send(SmtcEventSignal::TimelineProperties);
                    Ok(())
                }))?,
            );

            let _ = state_update_tx.try_send(Box::new(|state| {
                state.reset_to_empty();
                state.is_waiting_for_initial_update = true;
                log::trace!("[会话处理器] 正在等待 SMTC 第一次更新。");
            }));

            state.current_listener_tokens = Some(tokens);
            state.current_monitored_session = Some(new_s);

            // 切换后立即获取一次所有信息，确保UI快速更新
            log::info!("[会话处理器] 会话切换完成，正在获取所有初始状态。");
            let _ = smtc_event_tx.try_send(SmtcEventSignal::MediaProperties);
            let _ = smtc_event_tx.try_send(SmtcEventSignal::PlaybackInfo);
            let _ = smtc_event_tx.try_send(SmtcEventSignal::TimelineProperties);
        } else {
            // 3c. 没有活动会话了，重置状态
            log::info!("[会话处理器] 没有可用的媒体会话，将重置播放器状态。");
            let _ = state_update_tx.try_send(Box::new(|state| state.reset_to_empty()));
        }
    } else {
        log::debug!(
            "[会话处理器] 无需切换会话，目标与当前一致 ({:?})。",
            current_session_id.as_deref().unwrap_or("无")
        );
    }

    Ok(())
}

// --- 主监听器函数 ---

/// 运行 SMTC 监听器的主函数。
///
/// 这个函数在一个专用的系统线程 (`smtc_handler_thread`) 中被调用。它建立了一个复杂的运行环境来与 Windows SMTC API 安全交互：
/// 1.  **Tokio 单线程运行时**: 创建一个 `current_thread` 运行时和一个 `LocalSet`，以确保所有 `!Send` 的 WinRT 对象都在同一个线程上被 `await`。
/// 2.  **COM STA 初始化**: 在线程开始时初始化 COM 单线程套间 (STA)，这是与 UI 和事件相关的 WinRT API 的强制要求。
/// 3.  **Win32 消息泵**: 在后台启动一个标准的 `GetMessageW` / `DispatchMessageW` 循环。这是接收来自操作系统的 SMTC 事件回调所必需的。
/// 4.  **异步事件循环**: 运行一个基于 `tokio::select!` 的主循环，统一处理来自外部的控制命令、来自 SMTC 的事件信号以及后台异步任务的完成结果。
/// 5.  **优雅关闭**: 监听来自 `worker` 的关闭信号，并在退出前清理所有资源，包括注销事件监听器和停止消息泵。
///
/// # 参数
/// * `connector_update_tx` - 用于向 `MediaWorker` 发送状态更新的通道。
/// * `control_rx` - 用于从 `MediaWorker` 接收控制命令的通道。
/// * `player_state_arc` - 指向共享播放器状态的原子引用计数指针。
/// * `shutdown_rx` - 用于从 `MediaWorker` 接收关闭信号的通道。
pub fn run_smtc_listener(
    connector_update_tx: CrossbeamSender<InternalUpdate>,
    control_rx: CrossbeamReceiver<InternalCommand>,
    player_state_arc: Arc<TokioMutex<SharedPlayerState>>,
    shutdown_rx: CrossbeamReceiver<()>,
) -> Result<()> {
    log::info!("[SMTC Handler] 正在启动 SMTC 监听器");

    // 1. 初始化 Tokio 单线程运行时
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    let local_set = LocalSet::new();

    let (thread_id_tx, thread_id_rx) = std::sync::mpsc::channel::<u32>();

    // 2. 将所有逻辑包装在 local_set.block_on 中，确保它们运行在同一个 STA 线程上
    local_set.block_on(&rt, async move {
        // 使用 RAII Guard 确保 COM 在线程退出时被正确反初始化
        struct ComGuard;
        impl ComGuard {
            fn new() -> WinResult<Self> {
                // SAFETY: CoInitializeEx 必须在每个使用 COM/WinRT 的线程上调用一次。
                // 我们在此处初始化 STA，这是使用大多数 UI 和事件相关的 WinRT API 所必需的。
                // 这个调用是安全的，因为它在此线程的开始处被调用，且只调用一次。
                unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()? };
                Ok(Self)
            }
        }
        impl Drop for ComGuard {
            fn drop(&mut self) {
                // SAFETY: CoUninitialize 必须与 CoInitializeEx 成对出现。
                // 当 ComGuard 离开作用域时（即 async 块结束时），它会自动被调用，确保资源被正确释放。
                unsafe { CoUninitialize() };
                log::trace!("[COM Guard] CoUninitialize 已调用。");
            }
        }

        // 初始化 COM，如果失败则无法继续
        if let Err(e) = ComGuard::new() {
            log::error!("[SMTC Handler] COM 初始化失败 (STA): {e}，监听器线程无法启动。");
            return; // 没有 COM 无法继续
        }
        log::trace!("[SMTC Handler] COM (STA) 初始化成功。");


        // 3. 在后台线程中启动一个标准的 Win32 消息泵。
        // 这是接收 WinRT 事件所必需的。
        let message_pump_handle = tokio::task::spawn_blocking(move || {
            // SAFETY: GetCurrentThreadId 是安全的
            let thread_id = unsafe { GetCurrentThreadId() };
            // 将ID发送回主任务，如果失败则直接 panic，因为这是启动的关键步骤
            if thread_id_tx.send(thread_id).is_err() {
                log::error!("[消息泵] 无法发送线程ID，启动失败！");
                return;
            }

            log::trace!("[消息泵线程] 启动...");
            // SAFETY: 这是标准的 Win32 消息循环。
            // GetMessageW 会在没有消息时阻塞线程，将 CPU 使用率降至 0。
            // 当它收到 PostThreadMessageW 发送的 WM_QUIT 消息时，会返回 0，循环结束。
            // 它是线程安全的，因为它只处理属于这个线程的消息队列。
            unsafe {
                let mut msg = MSG::default();
                while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            log::trace!("[消息泵线程] 收到 WM_QUIT，已退出。");
        });

        let pump_thread_id = match thread_id_rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(id) => id,
            Err(_) => {
                log::error!("[SMTC 主循环] 等待消息泵线程ID超时，启动失败！");
                return;
            }
        };
        log::debug!("[SMTC 主循环] 已成功获取消息泵线程ID: {pump_thread_id}");

        // 4. 设置所有异步通信通道
        let (smtc_event_tx, mut smtc_event_rx) = tokio_channel::<SmtcEventSignal>(32);
        let (async_result_tx, mut async_result_rx) = tokio_channel::<AsyncTaskResult>(32);
        let (state_update_tx, mut state_update_rx) = tokio_channel::<StateUpdateFn>(32);

        // 5. 启动状态管理器 Actor
        let state_manager_handle = {
            let player_state_clone = player_state_arc.clone();
            let connector_update_tx_clone = connector_update_tx.clone();
            // 缓存上一次发送的状态，用于比较
            let mut last_sent_state_cache: Option<NowPlayingInfo> = None;

            tokio::task::spawn_local(async move {
                log::info!("[状态管理器 Actor] 任务已启动。");
                while let Some(update_fn) = state_update_rx.recv().await {
                    let mut state_guard = player_state_clone.lock().await;
                    update_fn(&mut state_guard);

                    let current_state = NowPlayingInfo::from(&*state_guard);

                    // 只有在状态发生有意义的变化时才发送事件
                    if Some(&current_state) != last_sent_state_cache.as_ref() {
                        log::trace!("[状态管理器 Actor] 检测到状态变化，发送 TrackChanged 事件。");
                        let update_to_send = InternalUpdate::NowPlayingTrackChanged(current_state.clone());
                        if connector_update_tx_clone.send(update_to_send).is_err() {
                            log::warn!("[状态管理器 Actor] 无法发送状态更新通知。");
                            break;
                        }
                        last_sent_state_cache = Some(current_state);
                    }
                }
                log::warn!("[状态管理器 Actor] 状态更新通道已关闭，任务退出。");
            })
        };

        // 6. 桥接同步通道到异步世界
        // 外部传入的是 std::sync::mpsc，我们需要在阻塞任务中将其消息转发到 tokio channel，
        // 以便在主循环的 `select!` 中使用。
        let (control_tx_async, mut control_rx_async) =
            tokio_channel::<InternalCommand>(32);
        tokio::task::spawn_blocking(move || {
            while let Ok(cmd) = control_rx.recv() {
                if control_tx_async.blocking_send(cmd).is_err() {
                    log::info!("[控制桥接] 目标通道已关闭，停止转发。");
                    break;
                }
            }
        });
        let (shutdown_tx_async, mut shutdown_rx_async) = tokio_channel::<()>(1);
        tokio::task::spawn_blocking(move || {
            if shutdown_rx.recv().is_ok() {
                let _ = shutdown_tx_async.blocking_send(());
            }
        });

        // 7. 初始化主循环状态和 SMTC 管理器
        let mut state = SmtcState::new();

        // 异步请求 SMTC 管理器，这是所有操作的起点
        log::debug!("[SMTC 主循环] 正在异步请求 SMTC 管理器...");
        spawn_async_op(
            MediaSessionManager::RequestAsync(),
            &async_result_tx,
            AsyncTaskResult::ManagerReady,
        );

        log::debug!("[SMTC 主循环] 进入 select! 事件循环...");

        // 8. 核心事件循环
        loop {
            tokio::select! {
                biased; // 优先处理关闭信号

                // 分支 1: 处理关闭信号
                _ = shutdown_rx_async.recv() => {
                    log::info!("[SMTC 主循环] 收到关闭信号，准备退出...");
                    break;
                },

                // 分支 2: 处理来自外部的控制命令
                Some(command) = control_rx_async.recv() => {
                    log::debug!("[SMTC 主循环] 收到外部命令: {command:?}");
                    match command {
                        InternalCommand::SetTextConversion(mode) => {
                            if state.text_conversion_mode != mode {
                                log::info!("[SMTC 主循环] 切换文本转换模式 -> {mode:?}");
                                state.text_conversion_mode = mode;

                                // 根据模式名称获取 OpenCC 配置文件名
                                let config_name = match mode {
                                    TextConversionMode::Off => None,
                                    TextConversionMode::TraditionalToSimplified => Some("t2s.json"),
                                    TextConversionMode::SimplifiedToTraditional => Some("s2t.json"),
                                    TextConversionMode::SimplifiedToTaiwan => Some("s2tw.json"),
                                    TextConversionMode::TaiwanToSimplified => Some("tw2s.json"),
                                    TextConversionMode::SimplifiedToHongKong => Some("s2hk.json"),
                                    TextConversionMode::HongKongToSimplified => Some("hk2s.json"),
                                };

                                // 创建新的转换器实例
                                state.text_converter = config_name.and_then(|name| {
                                    match OpenCC::from_config_name(name) {
                                        Ok(converter) => Some(converter),
                                        Err(e) => {
                                            log::error!("[SMTC 主循环] 加载 OpenCC 配置 '{name}' 失败: {e}");
                                            None
                                        }
                                    }
                                });

                                // 触发一次属性更新，以使用新的转换器重新转换当前显示的文本
                                let _ = smtc_event_tx.try_send(SmtcEventSignal::MediaProperties);
                            }
                        },
                        InternalCommand::SelectSmtcSession(id) => {
                            let new_target = if id.is_empty() { None } else { Some(id) };
                            log::info!("[SMTC 主循环] 切换目标会话 -> {new_target:?}");
                            state.target_session_id = new_target;
                            let _ = smtc_event_tx.try_send(SmtcEventSignal::Sessions);
                        }
                        InternalCommand::MediaControl(media_cmd) => {
                            let Some(session) = &state.current_monitored_session else {
                                continue;
                            };
                            log::debug!("[SMTC 主循环] 正在执行媒体控制指令: {media_cmd:?}");
                            match media_cmd {
                                SmtcControlCommand::SetVolume(level) => {
                                    if let Ok(id_hstr) = session.SourceAppUserModelId() {
                                        let session_id_str = hstring_to_string(&id_hstr);
                                        if !session_id_str.is_empty() {
                                            if let Some(old_task) = state.active_volume_easing_task.take() {
                                                old_task.abort();
                                            }
                                            let task_id = state.next_easing_task_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                            state.active_volume_easing_task = Some(tokio::task::spawn_local(
                                                volume_easing_task(
                                                    task_id,
                                                    level,
                                                    session_id_str,
                                                    connector_update_tx.clone()
                                                )
                                            ));
                                        }
                                    }
                                }
                                SmtcControlCommand::SetShuffle(is_active) => {
                                        let async_op = session.TryChangeShuffleActiveAsync(is_active);
                                        spawn_async_op(async_op, &async_result_tx, move |res| {
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
                                         spawn_async_op(async_op, &async_result_tx, move |res| {
                                            AsyncTaskResult::MediaControlCompleted(media_cmd, res)
                                        });
                                    }
                                other_cmd => {
                                    let async_op = match other_cmd {
                                        SmtcControlCommand::Play => session.TryPlayAsync(),
                                        SmtcControlCommand::Pause => session.TryPauseAsync(),
                                        SmtcControlCommand::SkipNext => session.TrySkipNextAsync(),
                                        SmtcControlCommand::SkipPrevious => session.TrySkipPreviousAsync(),
                                        SmtcControlCommand::SeekTo(pos) => session.TryChangePlaybackPositionAsync(pos as i64 * 10000),
                                        _ => unreachable!(),
                                    };
                                    spawn_async_op(async_op, &async_result_tx, move |res| {
                                        AsyncTaskResult::MediaControlCompleted(other_cmd, res)
                                    });
                                }
                            }
                        }
                        InternalCommand::RequestStateUpdate => {
                            log::info!("[SMTC 主循环] 收到状态更新请求，将重新获取所有状态。");
                            if let Some(old_session) = state.current_monitored_session.take()
                                && let Some(tokens) = state.current_listener_tokens.take() {
                                    log::debug!("[SMTC 主循环] 正在注销旧会话的事件监听器...");
                                    let _ = old_session.RemoveMediaPropertiesChanged(tokens.0);
                                    let _ = old_session.RemovePlaybackInfoChanged(tokens.1);
                                    let _ = old_session.RemoveTimelinePropertiesChanged(tokens.2);
                                }

                            if let Err(e) = handle_sessions_changed(
                                &mut state,
                                &connector_update_tx,
                                &smtc_event_tx,
                                &state_update_tx,
                            ) {
                                log::error!("[SMTC 主循环] 重建会话时出错: {e}");
                            }
                        }
                        InternalCommand::SetProgressTimer(enabled) => {
                            if enabled {
                                if state.active_progress_timer_task.is_none() {
                                    log::info!("[SMTC 主循环] 启用进度计时器。");
                                    let handle = tokio::task::spawn_local(progress_timer_task(
                                        player_state_arc.clone(),
                                        connector_update_tx.clone(),
                                    ));
                                    state.active_progress_timer_task = Some(handle);
                                }
                            } else if let Some(task) = state.active_progress_timer_task.take() {
                                log::info!("[SMTC 主循环] 禁用进度计时器。");
                                task.abort();
                            }
                        }
                    }
                },
                // 分支 3: 处理内部的 WinRT 事件信号
                Some(signal) = smtc_event_rx.recv() => {
                    log::trace!("[SMTC 主循环] 收到内部事件信号: {signal:?}");
                    match signal {
                        SmtcEventSignal::Sessions => {
                            if state.manager.is_some()
                                && let Err(e) = handle_sessions_changed(
                                    &mut state,
                                    &connector_update_tx,
                                    &smtc_event_tx,
                                    &state_update_tx,
                                ) {
                                    log::error!("[SMTC 主循环] 处理会话变更时出错: {e}");
                                }
                        }
                        SmtcEventSignal::MediaProperties => {
                            if let Some(s) = &state.current_monitored_session {
                                spawn_async_op(
                                    s.TryGetMediaPropertiesAsync(),
                                    &async_result_tx,
                                    AsyncTaskResult::MediaPropertiesReady,
                                );
                            }
                        }
                        SmtcEventSignal::PlaybackInfo => {
                            if let Some(s) = &state.current_monitored_session {
                                match s.GetPlaybackInfo() {
                                    Ok(info) => {
                                        let is_playing_now = info.PlaybackStatus().map_or_else(
                                            |e| {
                                                log::warn!("[State Update] 获取 PlaybackStatus 失败: {e:?}, 默认为 Paused");
                                                false
                                            },
                                            |status| status == GlobalSystemMediaTransportControlsSessionPlaybackStatus::Playing,
                                        );

                                        let is_shuffle_now = info.IsShuffleActive()
                                            .and_then(|iref| iref.Value())
                                            .unwrap_or(false);

                                        let repeat_mode_now = info.AutoRepeatMode()
                                            .and_then(|iref| iref.Value())
                                            .map(|rm| match rm {
                                                MediaPlaybackAutoRepeatMode::None => RepeatMode::Off,
                                                MediaPlaybackAutoRepeatMode::Track => RepeatMode::One,
                                                MediaPlaybackAutoRepeatMode::List => RepeatMode::All,
                                                _ => RepeatMode::Off,
                                            })
                                            .unwrap_or(RepeatMode::Off);

                                        let update_fn = Box::new(move |state: &mut SharedPlayerState| {
                                            if state.is_playing != is_playing_now {
                                                log::trace!("[State Update] 播放状态改变: {} -> {}", state.is_playing, is_playing_now);
                                                let estimated_pos = state.get_estimated_current_position_ms();
                                                state.last_known_position_ms = estimated_pos;
                                                state.last_known_position_report_time = Some(Instant::now());
                                            }
                                            state.is_playing = is_playing_now;
                                            state.is_shuffle_active = is_shuffle_now;
                                            state.repeat_mode = repeat_mode_now;

                                            if let Ok(c) = info.Controls() {
                                                state.can_pause = c.IsPauseEnabled().unwrap_or(false);
                                                state.can_play = c.IsPlayEnabled().unwrap_or(false);
                                                state.can_skip_next = c.IsNextEnabled().unwrap_or(false);
                                                state.can_skip_previous = c.IsPreviousEnabled().unwrap_or(false);
                                                state.can_seek = c.IsPlaybackPositionEnabled().unwrap_or(false);
                                            } else {
                                                log::warn!("[State Update] 获取媒体控件 (Controls) 失败。");
                                            }
                                        });

                                        match state_update_tx.try_send(update_fn) {
                                            Ok(_) => {} // 发送成功
                                            Err(TrySendError::Full(_)) => {
                                                log::warn!("[SMTC 主循环] 状态更新通道已满，丢弃了一次 PlaybackInfo 更新。");
                                            }
                                            Err(TrySendError::Closed(_)) => {
                                                log::error!("[SMTC 主循环] 状态更新通道已关闭，无法发送 PlaybackInfo 更新。");
                                            }
                                        }
                                    },
                                    Err(e) => log::warn!("[SMTC 主循环] 获取 PlaybackInfo 失败: {e:?}")
                                }
                            }
                        }
                        SmtcEventSignal::TimelineProperties => {
                            if let Some(s) = &state.current_monitored_session {
                                match s.GetTimelineProperties() {
                                    Ok(props) => {
                                        let pos_ms = props.Position().map_or(0, |d| (d.Duration / 10000) as u64);
                                        let dur_ms = props.EndTime().map_or(0, |d| (d.Duration / 10000) as u64);

                                        let update_fn = Box::new(move |state: &mut SharedPlayerState| {
                                            state.last_known_position_ms = pos_ms;
                                            if dur_ms > 0 { state.song_duration_ms = dur_ms; }
                                            state.last_known_position_report_time = Some(Instant::now());

                                            if state.is_waiting_for_initial_update {
                                                state.is_waiting_for_initial_update = false;
                                                log::trace!("[SMTC 主循环] 收到第一次 SMTC 更新，已清除等待标志。");
                                            }
                                        });

                                        match state_update_tx.try_send(update_fn) {
                                            Ok(_) => {} // Success
                                            Err(TrySendError::Full(_)) => {
                                                log::warn!("[SMTC 主循环] 状态更新通道已满，丢弃了一次 TimelineProperties 更新。");
                                            }
                                            Err(TrySendError::Closed(_)) => {
                                                log::error!("[SMTC 主循环] 状态更新通道已关闭，无法发送 TimelineProperties 更新。");
                                            }
                                        }
                                    },
                                    Err(e) => log::warn!("[SMTC 主循环] 获取 TimelineProperties 失败: {e:?}")
                                }
                            }
                        }
                    }
                }

                // 分支 4: 处理后台异步任务的结果
                Some(result) = async_result_rx.recv() => {
                    log::trace!("[SMTC 主循环] 收到异步任务结果。");
                    match result {
                        AsyncTaskResult::ManagerReady(Ok(mgr)) => {
                            log::info!("[SMTC 主循环] SMTC 管理器已就绪。");
                            let tx = smtc_event_tx.clone();
                            state.manager_sessions_changed_token = mgr.SessionsChanged(&TypedEventHandler::new(move |_, _| {
                                log::trace!("[Event] SessionsChanged 信号触发");
                                let _ = tx.try_send(SmtcEventSignal::Sessions);
                                Ok(())
                             })).ok();
                            state.manager = Some(mgr);
                            // 管理器就绪后，立即触发一次会话检查
                            match smtc_event_tx.try_send(SmtcEventSignal::Sessions) {
                                Ok(_) => {} // 发送成功
                                Err(TrySendError::Full(_)) => {
                                    log::warn!("[SMTC 主循环] 准备触发初始会话检查时发现事件通道已满。");
                                }
                                Err(TrySendError::Closed(_)) => {
                                    log::error!("[SMTC 主循环] 准备触发初始会话检查时发现事件通道已关闭。");
                                }
                            }
                        }
                        AsyncTaskResult::ManagerReady(Err(e)) => {
                            log::error!("[SMTC 主循环] 初始化 SMTC 管理器失败: {e:?}，监听器将关闭。");
                            break; // 致命错误，退出循环
                        }
                        AsyncTaskResult::MediaPropertiesReady(Ok(props)) => {
                            let get_prop_string = |prop_res: WinResult<HSTRING>, name: &str| {
                                prop_res.map_or_else(
                                    |e| {
                                        log::warn!("[SMTC 主循环] 获取媒体属性 '{name}' 失败: {e:?}");
                                        String::new()
                                    },
                                    |hstr| {
                                        crate::utils::convert_text(
                                            &hstring_to_string(&hstr),
                                            state.text_converter.as_ref(),
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
                                let update_fn = Box::new(move |state: &mut SharedPlayerState| {
                                    log::info!("[SMTC 主循环] 接收到新曲目信息: '{}' - '{}'", &artist, &title);
                                    if state.title != title || state.artist != artist {
                                        state.cover_data_hash = None;
                                        state.is_waiting_for_initial_update = true;
                                    }
                                    state.title = title;
                                    state.artist = artist;
                                    state.album = album;
                                });
                                // 发送文本信息更新
                                if state_update_tx.try_send(update_fn).is_err() {
                                    log::warn!("[SMTC 主循环] 状态更新通道已满，丢弃了一次文本信息更新。");
                                }
                            }

                            // 封面获取逻辑
                            if let Some(old_task) = state.active_cover_fetch_task.take() {
                                old_task.abort();
                            }

                            if let Ok(thumb_ref) = props.Thumbnail() {
                                let new_handle = tokio::task::spawn_local(fetch_cover_data_async(
                                    thumb_ref,
                                    state_update_tx.clone(),
                                ));
                                state.active_cover_fetch_task = Some(new_handle);
                            }
                        }
                        AsyncTaskResult::MediaPropertiesReady(Err(e)) => {
                            log::warn!("[SMTC 主循环] 获取媒体属性失败: {e:?}");
                        }
                        AsyncTaskResult::MediaControlCompleted(cmd, res) => {
                            match res {
                                Ok(true) => log::info!("[SMTC 主循环] 媒体控制指令 {cmd:?} 成功执行。"),
                                Ok(false) => log::warn!("[SMTC 主循环] 媒体控制指令 {cmd:?} 执行失败 (返回 false)。"),
                                Err(e) => log::warn!("[SMTC 主循环] 媒体控制指令 {cmd:?} 异步调用失败: {e:?}"),
                            }
                        }
                    }
                }
            }
        }

        // --- 优雅关闭 ---
        log::info!("[SMTC 主循环] 正在清理资源...");

        // 注销所有事件监听器
        if let Some(mgr) = state.manager
            && let Some(token) = state.manager_sessions_changed_token {
                let _ = mgr.RemoveSessionsChanged(token);
            }
        if let Some(session) = state.current_monitored_session
            && let Some(tokens) = state.current_listener_tokens {
                let _ = session.RemoveMediaPropertiesChanged(tokens.0);
                let _ = session.RemovePlaybackInfoChanged(tokens.1);
                let _ = session.RemoveTimelinePropertiesChanged(tokens.2);
            }
        // 中断正在运行的任务
        if let Some(task) = state.active_volume_easing_task {
            task.abort();
        }
        if let Some(task) = state.active_cover_fetch_task.take() {
             task.abort();
        }
        if let Some(task) = state.active_progress_timer_task.take() {
            task.abort();
        }
        state_manager_handle.abort();

        // 发送 WM_QUIT 消息来停止消息泵线程
        // SAFETY: PostThreadMessageW 是向特定线程发送消息的标准 Win32 API。
        // 我们在启动时获取了正确的线程ID `main_thread_id`，所以这是安全的。
        // 这是从外部线程安全地与消息循环交互的推荐方式。
        unsafe {
            use windows::Win32::Foundation::{LPARAM, WPARAM};
            use windows::Win32::UI::WindowsAndMessaging::PostThreadMessageW;

            if PostThreadMessageW(pump_thread_id, WM_QUIT, WPARAM(0), LPARAM(0)).is_err() {
                log::error!("[SMTC 清理] 发送 WM_QUIT 消息到消息泵线程 (ID:{pump_thread_id}) 失败。");
            }
        }
        if let Err(e) = message_pump_handle.await {
            log::warn!("[SMTC 清理] 等待消息泵线程退出时出错: {e:?}");
        }
    });

    log::info!("[SMTC Handler] 监听器线程已完全退出。");
    Ok(())
}
