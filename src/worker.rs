use std::{sync::Arc, thread};

use tokio::{
    runtime::Runtime,
    sync::{
        mpsc::{Receiver as TokioReceiver, Sender as TokioSender, channel},
        watch,
    },
    task::LocalSet,
};

use crate::{
    api::{
        DiagnosticInfo, MediaCommand, MediaUpdate, NowPlayingInfo, SmtcControlCommand,
        SmtcSessionInfo, TextConversionMode,
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
    /// 由 `smtc_handler` 发出，表示可用的 SMTC 会话列表已更新。
    SmtcSessionListChanged(Vec<SmtcSessionInfo>),
    /// 由 `smtc_handler` 发出，表示之前选中的会话已消失。
    SelectedSmtcSessionVanished(String),
    /// 由 `audio_capturer` 发出，包含一个捕获到的音频数据包。
    AudioDataPacket(Vec<u8>),
    /// 由 `audio_capturer` 发出，代表一个错误。
    AudioCaptureError(String),
    /// 由 `volume_control` (通过 `smtc_handler` 转发) 发出，表示某个应用的音量已变化。
    AudioSessionVolumeChanged {
        session_id: String,
        volume: f32,
        is_muted: bool,
    },
}

pub(crate) struct MediaWorker {
    command_rx: TokioReceiver<MediaCommand>,
    update_tx: TokioSender<MediaUpdate>,
    smtc_control_tx: Option<TokioSender<InternalCommand>>,
    smtc_update_rx: TokioReceiver<InternalUpdate>,
    diagnostics_rx: Option<TokioReceiver<DiagnosticInfo>>,
    now_playing_rx: watch::Receiver<NowPlayingInfo>,
    smtc_shutdown_tx: Option<TokioSender<()>>,
    smtc_handler_thread_handle: Option<thread::JoinHandle<()>>,
    audio_capturer: Option<AudioCapturer>,
    audio_update_rx: Option<TokioReceiver<InternalUpdate>>,
    tokio_runtime: Arc<Runtime>,
    _shared_player_state: Arc<tokio::sync::Mutex<SharedPlayerState>>,
}

impl MediaWorker {
    pub(crate) async fn run(
        command_rx: TokioReceiver<MediaCommand>,
        update_tx: TokioSender<MediaUpdate>,
    ) -> Result<()> {
        log::info!("[MediaWorker] Worker 正在启动...");

        let tokio_runtime = Arc::new(Runtime::new()?);
        let (smtc_update_tx, smtc_update_rx) = channel::<InternalUpdate>(32);
        let (smtc_control_tx, smtc_control_rx) = channel::<InternalCommand>(32);
        let (now_playing_tx, now_playing_rx) = watch::channel(NowPlayingInfo::default());
        let (diagnostics_tx, diagnostics_rx) = channel::<DiagnosticInfo>(32);

        let mut worker_instance = Self {
            command_rx,
            update_tx,
            smtc_control_tx: Some(smtc_control_tx),
            smtc_update_rx,
            diagnostics_rx: Some(diagnostics_rx),
            now_playing_rx,
            smtc_shutdown_tx: None,
            smtc_handler_thread_handle: None,
            audio_capturer: None,
            audio_update_rx: None,
            tokio_runtime: tokio_runtime.clone(),
            _shared_player_state: Arc::new(tokio::sync::Mutex::new(SharedPlayerState::default())),
        };

        worker_instance.start_smtc_handler_thread(
            smtc_control_rx,
            smtc_update_tx,
            now_playing_tx,
            diagnostics_tx,
        );

        log::debug!("[MediaWorker] 初始化完成，即将进入核心异步事件循环。");

        worker_instance.main_event_loop().await;

        log::trace!("[MediaWorker] 核心事件循环已退出，正在执行清理...");
        worker_instance.shutdown_all_subsystems().await;

        log::trace!("[MediaWorker] 核心事件循环已退出，工作线程即将终止。");
        Ok(())
    }

    async fn main_event_loop(&mut self) {
        loop {
            tokio::select! {
                biased;

                Some(command) = self.command_rx.recv() => {
                    log::trace!("[MediaWorker] 收到外部命令: {command:?}");
                    if let MediaCommand::Shutdown = command {
                        log::debug!("[MediaWorker] 收到外部关闭命令，准备退出...");
                        break;
                    }
                    self.handle_command_from_app(command).await;
                },

                Ok(_) = self.now_playing_rx.changed() => {
                    let info = self.now_playing_rx.borrow().clone();
                    if info.title.is_some()
                        && self.update_tx.send(MediaUpdate::TrackChanged(info)).await.is_err() {
                            log::error!("[MediaWorker] 发送 TrackChanged 更新到外部失败。");
                        }
                },

                Some(update) = self.smtc_update_rx.recv() => {
                    self.handle_internal_update(update, "SMTC").await;
                },

                maybe_diag_update = async {
                    if let Some(rx) = self.diagnostics_rx.as_mut() {
                        rx.recv().await
                    } else {
                        std::future::pending().await
                    }
                }, if self.diagnostics_rx.is_some() => {
                     if let Some(diag_info) = maybe_diag_update
                        && self.update_tx.send(MediaUpdate::Diagnostic(diag_info)).await.is_err() {
                            log::error!("[MediaWorker] 发送 Diagnostic 更新到外部失败。");
                        }
                },

                maybe_audio_update = async {
                    if let Some(rx) = self.audio_update_rx.as_mut() {
                        rx.recv().await
                    } else {
                        std::future::pending().await
                    }
                }, if self.audio_update_rx.is_some() => {
                    if let Some(update) = maybe_audio_update {
                         self.handle_internal_update(update, "Audio").await;
                    } else {
                        log::warn!("[MediaWorker] 音频捕获更新通道已断开 (线程可能已退出)。");
                        let _ = self.update_tx.send(MediaUpdate::Error("音频捕获异常".to_string())).await;
                        self.stop_audio_capture_internal();
                    }
                }
            }
        }
    }

    async fn handle_command_from_app(&mut self, command: MediaCommand) {
        match command {
            MediaCommand::SelectSession(session_id) => {
                self.send_internal_command_to_smtc(InternalCommand::SelectSmtcSession(session_id))
                    .await;
            }
            MediaCommand::Control(smtc_cmd) => {
                self.send_internal_command_to_smtc(InternalCommand::MediaControl(smtc_cmd))
                    .await;
            }
            MediaCommand::StartAudioCapture => {
                self.start_audio_capture_internal();
            }
            MediaCommand::StopAudioCapture => {
                self.stop_audio_capture_internal();
            }
            MediaCommand::SetTextConversion(mode) => {
                self.send_internal_command_to_smtc(InternalCommand::SetTextConversion(mode))
                    .await;
            }
            MediaCommand::RequestUpdate => {
                self.send_internal_command_to_smtc(InternalCommand::RequestStateUpdate)
                    .await;
            }
            MediaCommand::SetHighFrequencyProgressUpdates(enabled) => {
                self.send_internal_command_to_smtc(InternalCommand::SetProgressTimer(enabled))
                    .await;
            }
            MediaCommand::Shutdown => {
                // 已在上面循环中优先处理
            }
        }
    }

    async fn handle_internal_update(&mut self, internal_update: InternalUpdate, source: &str) {
        let public_update: MediaUpdate = internal_update.into();

        if self.update_tx.send(public_update).await.is_err() {
            log::error!("[MediaWorker] 发送更新 (来自 {source}) 到外部失败。");
        }
    }

    async fn send_internal_command_to_smtc(&self, command: InternalCommand) {
        if let Some(sender) = &self.smtc_control_tx {
            if sender.send(command).await.is_err() {
                log::error!("[MediaWorker] 发送命令到 SMTC 处理器失败。");
            }
        } else {
            log::error!("[MediaWorker] SMTC 控制通道无效，无法发送命令。");
        }
    }

    fn start_smtc_handler_thread(
        &mut self,
        control_rx_for_smtc: TokioReceiver<InternalCommand>,
        update_tx_for_smtc: TokioSender<InternalUpdate>,
        now_playing_tx: watch::Sender<NowPlayingInfo>,
        diagnostics_tx: TokioSender<DiagnosticInfo>,
    ) {
        if self.smtc_handler_thread_handle.is_some() {
            log::warn!("[MediaWorker] 尝试启动 SMTC 处理器，但它似乎已在运行。");
            return;
        }

        log::debug!("[MediaWorker] 正在启动 SMTC Handler 线程...");
        let player_state_clone = Arc::clone(&self._shared_player_state);

        let (shutdown_tx, shutdown_rx_for_smtc) = channel::<()>(1);
        self.smtc_shutdown_tx = Some(shutdown_tx);

        let handle = thread::Builder::new()
            .name("smtc_handler_thread".to_string())
            .spawn(move || {
                log::debug!("[SMTC Handler Thread] 线程已启动。");
                if let Err(e) = smtc_handler::run_smtc_listener(
                    update_tx_for_smtc,
                    control_rx_for_smtc,
                    player_state_clone,
                    shutdown_rx_for_smtc,
                    now_playing_tx,
                    diagnostics_tx,
                ) {
                    log::error!("[SMTC Handler Thread] SMTC Handler 运行出错: {e}");
                }
                log::debug!("[SMTC Handler Thread] 线程已结束。");
            })
            .expect("无法启动 SMTC Handler 线程");
        self.smtc_handler_thread_handle = Some(handle);
    }

    async fn stop_smtc_handler_thread(&mut self) {
        if self.smtc_control_tx.take().is_some() {
            log::debug!("[MediaWorker] SMTC 控制通道已清理。");
        }

        if let Some(tx) = self.smtc_shutdown_tx.take() {
            log::debug!("[MediaWorker] 正在向 SMTC 处理器发送关闭信号...");
            if tx.send(()).await.is_err() {
                log::warn!("[MediaWorker] 发送关闭信号至 SMTC 处理器失败 (可能已自行关闭)。");
            }
        }

        if let Some(handle) = self.smtc_handler_thread_handle.take() {
            log::debug!("[MediaWorker] 正在等待 SMTC Handler 线程退出...");
            // 在异步函数中 join 阻塞操作需要使用 spawn_blocking
            let res = tokio::task::spawn_blocking(move || handle.join()).await;
            match res {
                Ok(Ok(_)) => log::debug!("[MediaWorker] SMTC Handler 线程已成功退出。"),
                Ok(Err(e)) => log::warn!("[MediaWorker] 等待 SMTC Handler 线程退出失败: {e:?}"),
                Err(e) => log::warn!("[MediaWorker] spawn_blocking 任务失败: {e:?}"),
            }
        }
    }

    fn start_audio_capture_internal(&mut self) {
        if self.audio_capturer.is_some() {
            log::warn!("[MediaWorker] 音频捕获已在运行，无需重复启动。");
            return;
        }
        log::debug!("[MediaWorker] 正在启动音频捕获...");

        let (audio_update_tx, audio_update_rx) = channel::<InternalUpdate>(256);

        let mut capturer = AudioCapturer::new();
        match capturer.start_capture(audio_update_tx) {
            Ok(_) => {
                self.audio_capturer = Some(capturer);
                self.audio_update_rx = Some(audio_update_rx);
                log::info!("[MediaWorker] 音频捕获已成功启动。");
            }
            Err(e) => {
                log::error!("[MediaWorker] 启动音频捕获失败: {e}");
                self.audio_capturer = None;
                self.audio_update_rx = None;
            }
        }
    }

    fn stop_audio_capture_internal(&mut self) {
        if let Some(mut capturer) = self.audio_capturer.take() {
            log::debug!("[MediaWorker] 正在停止音频捕获...");
            capturer.stop_capture();
        }
        if self.audio_update_rx.take().is_some() {
            log::debug!("[MediaWorker] 音频更新通道已清理。");
        }
    }

    async fn shutdown_all_subsystems(&mut self) {
        log::info!("[MediaWorker] 正在关闭所有子系统...");
        self.stop_smtc_handler_thread().await;
        self.stop_audio_capture_internal();
    }
}

impl Drop for MediaWorker {
    fn drop(&mut self) {
        if self.smtc_control_tx.is_some() {
            log::warn!(
                "[MediaWorker] MediaWorker 实例被意外丢弃 (可能发生 panic)，正在尝试关闭所有子系统..."
            );
            let rt = Arc::clone(&self.tokio_runtime);
            rt.block_on(self.shutdown_all_subsystems());
        } else {
            log::trace!("[MediaWorker] MediaWorker 实例被正常丢弃。");
        }
    }
}

pub(crate) fn start_media_worker_thread(
    command_rx: TokioReceiver<MediaCommand>,
    update_tx: TokioSender<MediaUpdate>,
) -> Result<thread::JoinHandle<()>> {
    thread::Builder::new()
        .name("media_worker_thread".to_string())
        .spawn(move || {
            let local_set = LocalSet::new();
            let rt = Runtime::new().expect("无法为 MediaWorker 创建 Tokio 运行时");
            local_set.block_on(&rt, async {
                if let Err(e) = MediaWorker::run(command_rx, update_tx).await {
                    log::error!("[MediaWorker Thread] Worker 运行失败: {e}");
                }
            });
        })
        .map_err(|e| SmtcError::WorkerThread(e.to_string()))
}

impl From<InternalUpdate> for MediaUpdate {
    fn from(internal: InternalUpdate) -> Self {
        match internal {
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
            InternalUpdate::AudioCaptureError(err) => MediaUpdate::Error(err),
        }
    }
}
