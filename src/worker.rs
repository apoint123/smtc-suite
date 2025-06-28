use std::{sync::Arc, thread};

use crossbeam_channel::{Receiver as CrossbeamReceiver, Sender as CrossbeamSender};
use tokio::{runtime::Runtime, sync::mpsc::Receiver as TokioReceiver, task::LocalSet};

use crate::{
    api::{
        MediaCommand, MediaUpdate, NowPlayingInfo, SmtcControlCommand, SmtcSessionInfo,
        TextConversionMode,
    },
    audio_capture::AudioCapturer,
    error::{Result, SmtcError},
    smtc_handler::{self, SharedPlayerState},
};

/// 在 `MediaWorker` 内部使用的命令，用于控制其子模块。
///
/// 这个枚举定义了 `MediaWorker` 与其管理的后台任务（如 `smtc_handler`）之间的通信协议。
/// 它将来自外部公共 API (`MediaCommand`) 的意图，转换为内部模块可以理解的具体指令。
#[derive(Debug, Clone)]
pub(crate) enum InternalCommand {
    /// 指示 `smtc_handler` 切换到指定的媒体会话。
    SelectSmtcSession(String),
    /// 向 `smtc_handler` 转发一个媒体控制指令（如播放、暂停等）。
    MediaControl(SmtcControlCommand),
    /// 请求 `smtc_handler` 重新获取并广播其当前状态。
    RequestStateUpdate,
    /// 设置 SMTC 元数据的文本转换模式。
    SetTextConversion(TextConversionMode),
    /// 指示 smtc_handler 启动或停止其内部的进度模拟计时器。
    SetProgressTimer(bool),
}

/// 在 `MediaWorker` 内部使用的更新事件，由其子模块发出。
///
/// 这个枚举代表了所有子模块可能产生的事件，`MediaWorker` 的主事件循环会监听这些事件，
/// 然后将它们转换为外部可见的 `MediaUpdate`。
#[derive(Debug, Clone)]
pub(crate) enum InternalUpdate {
    /// 由 `smtc_handler` 发出，表示当前播放的曲目信息已变更。
    NowPlayingTrackChanged(NowPlayingInfo),
    /// 强制刷新并发送当前的曲目信息。
    NowPlayingTrackChangedForced(NowPlayingInfo),
    /// 由 `smtc_handler` 发出，表示可用的 SMTC 会话列表已更新。
    SmtcSessionListChanged(Vec<SmtcSessionInfo>),
    /// 由 `smtc_handler` 发出，表示之前选中的会话已消失。
    SelectedSmtcSessionVanished(String),
    /// 由 `audio_capturer` 发出，包含一个捕获到的音频数据包。
    AudioDataPacket(Vec<u8>),
    /// 由 `volume_control` (通过 `smtc_handler` 转发) 发出，表示某个应用的音量已变化。
    AudioSessionVolumeChanged {
        session_id: String,
        volume: f32,
        is_muted: bool,
    },
}

/// `MediaWorker` 是整个媒体库的核心协调器。
///
/// 它在一个专用的后台线程中运行，负责：
/// 1.  **生命周期管理**: 启动、停止和监控 `smtc_handler` 和 `audio_capturer` 等子系统。
/// 2.  **事件驱动循环**: 在一个高效的异步事件循环中，使用 `tokio::select!` 统一处理所有来源的事件。
/// 3.  **命令分发**: 接收来自公共 API (`MediaController`) 的命令，并将其分发给相应的子模块。
/// 4.  **状态聚合与通知**: 从子模块收集更新，并将它们转换为公共的 `MediaUpdate` 事件，发送给调用方。
pub(crate) struct MediaWorker {
    // --- 核心 I/O ---
    /// 异步接收器，用于从“桥接任务”接收外部传入的 `MediaCommand`。
    command_rx: TokioReceiver<MediaCommand>,
    /// 同步发送器，用于向外部（`MediaController`）发送 `MediaUpdate`。
    update_tx: CrossbeamSender<MediaUpdate>,

    // --- SMTC 子系统 ---
    /// 向 SMTC 处理器线程发送内部控制命令。
    smtc_control_tx: Option<CrossbeamSender<InternalCommand>>,
    /// 从 SMTC 处理器的“桥接任务”异步接收内部更新。
    smtc_update_rx: TokioReceiver<InternalUpdate>,
    /// 向 SMTC 处理器线程发送关闭信号，用于优雅地停止它。
    smtc_shutdown_tx: Option<CrossbeamSender<()>>,
    /// SMTC 处理器线程的句柄，用于在关闭时 `join` 它。
    smtc_handler_thread_handle: Option<thread::JoinHandle<()>>,

    // --- 音频捕获子系统 ---
    /// 音频捕获器的实例。
    audio_capturer: Option<AudioCapturer>,
    /// 从音频捕获器的“桥接任务”异步接收音频数据更新。
    audio_update_rx: Option<TokioReceiver<InternalUpdate>>,

    // --- 运行环境与共享状态 ---
    /// Tokio 运行时实例的共享引用，用于在 `worker` 内部派生新的异步任务。
    tokio_runtime: Arc<Runtime>,
    /// 共享的播放器状态，主要传递给 `smtc_handler` 模块使用。
    /// `_` 前缀表示 `MediaWorker` 自身不直接操作它，仅作持有和传递。
    _shared_player_state: Arc<tokio::sync::Mutex<SharedPlayerState>>,
}

impl MediaWorker {
    /// 启动并运行 `MediaWorker` 的主逻辑。
    ///
    /// 这个函数是 `MediaWorker` 线程的入口点。它会完成所有必要的初始化，
    /// 包括创建 Tokio 异步环境、设置通信通道、启动桥接任务，并最终驱动核心事件循环。
    ///
    /// # 参数
    /// * `command_rx_from_app`: 从外部 `MediaController` 传入的同步命令接收器。
    /// * `update_tx_to_app`: 用于向外部 `MediaController` 发送更新的同步发送器。
    pub(crate) fn run(
        command_rx_from_app: CrossbeamReceiver<MediaCommand>,
        update_tx_to_app: CrossbeamSender<MediaUpdate>,
    ) -> Result<()> {
        log::info!("[MediaWorker] Worker 正在启动...");

        // 1. 创建 Tokio 运行时和 LocalSet。
        // LocalSet 对于运行那些不是 `Send` 的 Future 至关重要，例如与 Windows COM/WinRT 交互的任务。
        let tokio_runtime = Arc::new(Runtime::new()?);
        let local_set = LocalSet::new();

        // 2. 创建所有需要的通信通道。
        // a. 用于核心 select! 循环的【异步】通道。
        let (command_tx_async, command_rx_async) = tokio::sync::mpsc::channel::<MediaCommand>(32);
        let (smtc_update_tx_async, smtc_update_rx_async) =
            tokio::sync::mpsc::channel::<InternalUpdate>(32);

        // b. 用于与子模块（在它们自己的线程中运行）通信的【同步】通道。
        let (smtc_update_tx_sync, smtc_update_rx_sync) = crossbeam_channel::unbounded();
        let (smtc_control_tx_sync, smtc_control_rx_sync) = crossbeam_channel::unbounded();

        // 3. 启动所有“桥接”任务。
        // 这些任务是连接同步世界和异步世界的桥梁。它们在后台运行，
        // 阻塞地等待同步通道 (`std::sync::mpsc`) 上的消息，
        // 然后异步地将消息发送到 `tokio::sync::mpsc` 通道，以便 `select!` 循环可以 `await` 它们。
        log::debug!("[MediaWorker] 正在启动所有同步->异步通道桥接任务...");
        let rt_clone_for_bridges = tokio_runtime.clone();

        // 桥接：外部命令 (std) -> 内部事件循环 (tokio)
        rt_clone_for_bridges.spawn(async move {
            while let Ok(cmd) = command_rx_from_app.recv() {
                // 检查命令是否为 Shutdown，以便我们可以主动退出循环
                let is_shutdown = matches!(cmd, MediaCommand::Shutdown);

                if command_tx_async.send(cmd).await.is_err() {
                    log::error!("[桥接任务-主命令] 无法将命令发送至异步通道，接收端可能已关闭。");
                    break;
                }

                // 如果我们刚刚发送的是 Shutdown 命令，那么我们的任务也完成了，退出循环。
                if is_shutdown {
                    log::debug!("[桥接任务-主命令] 收到并转发了关闭命令，任务即将结束。");
                    break;
                }
            }
            log::debug!("[桥接任务-主命令] 通道已关闭，任务结束。");
        });

        // 桥接：SMTC 更新 (std) -> 内部事件循环 (tokio)
        rt_clone_for_bridges.spawn(async move {
            while let Ok(update) = smtc_update_rx_sync.recv() {
                if smtc_update_tx_async.send(update).await.is_err() {
                    log::error!("[桥接任务-SMTC更新] 无法将更新发送至异步通道。");
                    break;
                }
            }
            log::debug!("[桥接任务-SMTC更新] 通道已关闭，任务结束。");
        });

        // 4. 初始化 MediaWorker 实例。
        let mut worker_instance = Self {
            command_rx: command_rx_async,
            update_tx: update_tx_to_app,
            smtc_control_tx: Some(smtc_control_tx_sync),
            smtc_update_rx: smtc_update_rx_async,
            smtc_shutdown_tx: None,
            smtc_handler_thread_handle: None,
            audio_capturer: None,
            audio_update_rx: None,
            tokio_runtime: tokio_runtime.clone(),
            _shared_player_state: Arc::new(tokio::sync::Mutex::new(SharedPlayerState::default())),
        };

        // 5. 启动所有子系统。
        worker_instance.start_smtc_handler_thread(smtc_control_rx_sync, smtc_update_tx_sync);

        log::debug!("[MediaWorker] 初始化完成，即将进入核心异步事件循环。");

        // 6. 使用 `block_on` 在当前线程上驱动异步主循环，直到它完成。
        //    使用 `async move` 将 `worker_instance` 的所有权移入闭包，以解决借用冲突。
        local_set.block_on(&tokio_runtime, async move {
            worker_instance.main_event_loop().await;

            // 在事件循环结束后，但在函数返回之前，显式地关闭所有子系统。
            log::trace!("[MediaWorker] 核心事件循环已退出，正在执行清理...");
            worker_instance.shutdown_all_subsystems();
        });

        log::trace!("[MediaWorker] 核心事件循环已退出，工作线程即将终止。");
        Ok(())
    }

    /// `MediaWorker` 的核心异步事件循环。
    ///
    /// 使用 `tokio::select!` 宏以非阻塞方式高效地等待来自所有来源的事件。
    async fn main_event_loop(&mut self) {
        loop {
            tokio::select! {
                // `biased` 关键字确保 select! 总是按从上到下的顺序检查分支。
                // 这让我们可以优先处理更重要的事件，例如外部命令（特别是 Shutdown）。
                biased;

                // --- 分支 1: 处理来自外部 API 的命令 ---
                Some(command) = self.command_rx.recv() => {
                    log::trace!("[MediaWorker] 收到外部命令: {command:?}");
                    // 立即处理 Shutdown 命令以跳出循环
                    if let MediaCommand::Shutdown = command {
                        log::debug!("[MediaWorker] 收到外部关闭命令，准备退出...");
                        break;
                    }
                    self.handle_command_from_app(command);
                },

                // --- 分支 2: 处理来自 SMTC 处理器的更新 ---
                Some(update) = self.smtc_update_rx.recv() => {
                    self.handle_internal_update(update, "SMTC");
                },

                // --- 分支 3: 处理来自音频捕获器的更新 (如果已启动) ---
                // `if self.audio_update_rx.is_some()` 确保只在音频捕获启动时才监听此分支。
                maybe_audio_update = async {
                    if let Some(rx) = self.audio_update_rx.as_mut() {
                        rx.recv().await
                    } else {
                        // 如果音频捕获未运行，返回一个永远不会完成的 Future，
                        // 使该 select! 分支永远不会被触发。
                        std::future::pending().await
                    }
                }, if self.audio_update_rx.is_some() => {
                    if let Some(update) = maybe_audio_update {
                         self.handle_internal_update(update, "Audio");
                    } else {
                        // `audio_update_rx.recv()` 返回 `None`，表示通道已关闭。
                        log::warn!("[MediaWorker] 音频捕获更新通道已断开 (线程可能已退出)。");
                        let _ = self.update_tx.send(MediaUpdate::Error("音频捕获异常".to_string()));
                        self.stop_audio_capture_internal(); // 清理相关资源
                    }
                }
            }
        }
    }

    /// 辅助函数，用于处理从主应用接收到的单个命令。
    fn handle_command_from_app(&mut self, command: MediaCommand) {
        match command {
            MediaCommand::SelectSession(session_id) => {
                self.send_internal_command_to_smtc(InternalCommand::SelectSmtcSession(session_id));
            }
            MediaCommand::Control(smtc_cmd) => {
                self.send_internal_command_to_smtc(InternalCommand::MediaControl(smtc_cmd));
            }
            MediaCommand::StartAudioCapture => {
                self.start_audio_capture_internal();
            }
            MediaCommand::StopAudioCapture => {
                self.stop_audio_capture_internal();
            }
            MediaCommand::SetTextConversion(mode) => {
                self.send_internal_command_to_smtc(InternalCommand::SetTextConversion(mode));
            }
            MediaCommand::RequestUpdate => {
                self.send_internal_command_to_smtc(InternalCommand::RequestStateUpdate);
            }
            MediaCommand::SetHighFrequencyProgressUpdates(enabled) => {
                self.send_internal_command_to_smtc(InternalCommand::SetProgressTimer(enabled));
            }
            MediaCommand::Shutdown => {
                // 已在上面循环中优先处理
            }
        }
    }

    /// 辅助函数，用于处理来自子模块的内部更新，并将其转换为公共更新发送出去。
    fn handle_internal_update(&mut self, internal_update: InternalUpdate, source: &str) {
        let public_update: MediaUpdate = internal_update.into();

        if self.update_tx.send(public_update).is_err() {
            log::error!("[MediaWorker] 发送更新 (来自 {source}) 到外部失败。");
        }
    }

    /// 辅助函数，用于向 SMTC 处理器发送内部命令。
    fn send_internal_command_to_smtc(&self, command: InternalCommand) {
        if let Some(sender) = &self.smtc_control_tx {
            if sender.send(command).is_err() {
                log::error!("[MediaWorker] 发送命令到 SMTC 处理器失败。");
            }
        } else {
            log::error!("[MediaWorker] SMTC 控制通道无效，无法发送命令。");
        }
    }

    /// 启动 SMTC 处理程序线程，并建立与之通信的通道。
    fn start_smtc_handler_thread(
        &mut self,
        control_rx_for_smtc: CrossbeamReceiver<InternalCommand>,
        update_tx_for_smtc: CrossbeamSender<InternalUpdate>,
    ) {
        if self.smtc_handler_thread_handle.is_some() {
            log::warn!("[MediaWorker] 尝试启动 SMTC 处理器，但它似乎已在运行。");
            return;
        }

        log::debug!("[MediaWorker] 正在启动 SMTC Handler 线程...");
        let player_state_clone = Arc::clone(&self._shared_player_state);

        // 为 SMTC 处理器创建专用的关闭通道，以实现显式的生命周期管理。
        let (shutdown_tx, shutdown_rx_for_smtc) = crossbeam_channel::unbounded::<()>();
        self.smtc_shutdown_tx = Some(shutdown_tx);

        let handle = thread::Builder::new()
            .name("smtc_handler_thread".to_string())
            .spawn(move || {
                log::debug!("[SMTC Handler Thread] 线程已启动。");
                if let Err(e) = smtc_handler::run_smtc_listener(
                    update_tx_for_smtc,
                    control_rx_for_smtc,
                    player_state_clone,
                    shutdown_rx_for_smtc, // 传递关闭信号接收端
                ) {
                    log::error!("[SMTC Handler Thread] SMTC Handler 运行出错: {e}");
                }
                log::debug!("[SMTC Handler Thread] 线程已结束。");
            })
            .expect("无法启动 SMTC Handler 线程"); // 如果线程无法启动，直接 panic
        self.smtc_handler_thread_handle = Some(handle);
    }

    /// 优雅地停止 SMTC 处理程序线程。
    fn stop_smtc_handler_thread(&mut self) {
        // 1. **首先 drop 掉控制通道的发送端。**
        //    `take()` 会将 `Some(sender)` 变为 `None`，这个 sender 随即被 drop。
        //    一旦所有 sender (这里只有一个) 都被 drop，`smtc_handler` 中桥接任务的
        //    `control_rx.recv()` 就会立即返回 `Err(Disconnected)`，从而让桥接任务退出。
        if self.smtc_control_tx.take().is_some() {
            log::debug!("[MediaWorker] SMTC 控制通道已清理，将通知桥接任务退出。");
        }

        // 2. 发送明确的关闭信号。
        //    这会让 smtc_handler 的主 select! 循环退出。
        if let Some(tx) = self.smtc_shutdown_tx.take() {
            log::debug!("[MediaWorker] 正在向 SMTC 处理器发送关闭信号...");
            if tx.send(()).is_err() {
                log::warn!("[MediaWorker] 发送关闭信号至 SMTC 处理器失败 (可能已自行关闭)。");
            }
        }

        // 3. **最后再 join 线程。**
        //    此时，smtc_handler 的主循环和所有桥接任务都收到了退出信号或通道已关闭，
        //    它们都可以顺利地结束，所以 join() 不会再被阻塞。
        if let Some(handle) = self.smtc_handler_thread_handle.take() {
            log::debug!("[MediaWorker] 正在等待 SMTC Handler 线程退出...");
            match handle.join() {
                Ok(_) => log::debug!("[MediaWorker] SMTC Handler 线程已成功退出。"),
                Err(e) => log::warn!("[MediaWorker] 等待 SMTC Handler 线程退出失败: {e:?}"),
            }
        }
    }

    /// 启动音频捕获子系统。
    fn start_audio_capture_internal(&mut self) {
        if self.audio_capturer.is_some() {
            log::warn!("[MediaWorker] 音频捕获已在运行，无需重复启动。");
            return;
        }
        log::debug!("[MediaWorker] 正在启动音频捕获...");

        // 为音频捕获创建专用的 std 通道，并桥接到异步世界
        let (audio_update_tx_sync, audio_update_rx_sync) = std::sync::mpsc::channel();
        let (audio_update_tx_async, audio_update_rx_async) =
            tokio::sync::mpsc::channel::<InternalUpdate>(256);

        let rt_clone = self.tokio_runtime.clone();
        rt_clone.spawn(async move {
            log::debug!("[桥接任务-音频] 音频数据桥接任务已启动。");
            while let Ok(update) = audio_update_rx_sync.recv() {
                if audio_update_tx_async.send(update).await.is_err() {
                    log::warn!("[桥接任务-音频] 无法将音频数据包发送至异步通道。");
                    break;
                }
            }
            log::debug!("[桥接任务-音频] 通道已关闭，任务结束。");
        });

        let mut capturer = AudioCapturer::new();
        match capturer.start_capture(audio_update_tx_sync) {
            Ok(_) => {
                self.audio_capturer = Some(capturer);
                self.audio_update_rx = Some(audio_update_rx_async);
                log::info!("[MediaWorker] 音频捕获已成功启动。");
            }
            Err(e) => {
                log::error!("[MediaWorker] 启动音频捕获失败: {e}");
                // 确保清理
                self.audio_capturer = None;
                self.audio_update_rx = None;
            }
        }
    }

    /// 停止音频捕获子系统。
    fn stop_audio_capture_internal(&mut self) {
        if let Some(mut capturer) = self.audio_capturer.take() {
            log::debug!("[MediaWorker] 正在停止音频捕获...");
            capturer.stop_capture();
        }
        // 清理异步接收通道，这将导致 `select!` 宏中的对应分支不再被轮询。
        if self.audio_update_rx.take().is_some() {
            log::debug!("[MediaWorker] 音频更新通道已清理。");
        }
    }

    /// 统一关闭所有子系统，用于程序退出前的清理。
    fn shutdown_all_subsystems(&mut self) {
        log::info!("[MediaWorker] 正在关闭所有子系统...");
        self.stop_smtc_handler_thread();
        self.stop_audio_capture_internal();
    }
}

/// `MediaWorker` 的 `Drop` 实现。
///
/// 这是一个安全保障，确保即使 `MediaWorker` 的实例因为意外情况被丢弃时，
/// 其管理的子系统也能被尝试关闭，防止线程泄漏。
impl Drop for MediaWorker {
    fn drop(&mut self) {
        // 只有在正常关闭流程没有被执行的情况下，才执行备用关闭逻辑
        if self.smtc_control_tx.is_some() {
            log::warn!(
                "[MediaWorker] MediaWorker 实例被意外丢弃 (可能发生 panic)，正在关闭所有子系统..."
            );
            self.shutdown_all_subsystems();
        } else {
            // smtc_control_tx 是 None，说明正常关闭已执行
            log::trace!("[MediaWorker] MediaWorker 实例被正常丢弃。");
        }
    }
}

/// 启动 `MediaWorker` 后台线程的公共函数。
///
/// 这是从外部（`lib.rs`）创建和运行 `MediaWorker` 的标准方式。
/// 它会创建一个新的系统线程，并将所有权和控制权移交给 `MediaWorker::run`。
///
/// # 返回
/// - `Ok(JoinHandle)`: 成功启动后，返回线程的句柄。
/// - `Err(SmtcError)`: 如果创建线程失败。
pub(crate) fn start_media_worker_thread(
    command_rx: CrossbeamReceiver<MediaCommand>,
    update_tx: CrossbeamSender<MediaUpdate>,
) -> Result<thread::JoinHandle<()>> {
    thread::Builder::new()
        .name("media_worker_thread".to_string())
        .spawn(move || {
            if let Err(e) = MediaWorker::run(command_rx, update_tx) {
                log::error!("[MediaWorker Thread] Worker 运行失败: {e}");
            }
        })
        .map_err(|e| SmtcError::WorkerThread(e.to_string()))
}

/// 为 `InternalUpdate` 实现 `From` trait，以便能方便地转换为 `MediaUpdate`。
impl From<InternalUpdate> for MediaUpdate {
    fn from(internal: InternalUpdate) -> Self {
        match internal {
            InternalUpdate::NowPlayingTrackChanged(info) => MediaUpdate::TrackChanged(info),
            InternalUpdate::NowPlayingTrackChangedForced(info) => {
                MediaUpdate::TrackChangedForced(info)
            }
            InternalUpdate::SmtcSessionListChanged(list) => MediaUpdate::SessionsChanged(list),
            InternalUpdate::AudioDataPacket(bytes) => MediaUpdate::AudioData(bytes),
            InternalUpdate::AudioSessionVolumeChanged {
                session_id,
                volume,
                is_muted,
            } => MediaUpdate::VolumeChanged {
                session_id,
                volume,
                is_muted,
            },
            InternalUpdate::SelectedSmtcSessionVanished(session_id) => {
                MediaUpdate::SelectedSessionVanished(session_id)
            }
        }
    }
}
