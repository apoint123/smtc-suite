use std::{sync::Arc, thread};

use tokio::task::JoinHandle as TokioJoinHandle;
use tokio::{
    sync::mpsc::{Receiver as TokioReceiver, Sender as TokioSender, channel},
    task::LocalSet,
};
use windows::{
    Win32::{
        Media::Audio::{AudioSessionState, AudioSessionStateExpired},
        System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize},
    },
    core::Result as WinResult,
};

use crate::{
    api::SharedPlayerState,
    api::{
        DiagnosticInfo, MediaCommand, MediaUpdate, NowPlayingInfo, SmtcControlCommand,
        SmtcSessionInfo, TextConversionMode,
    },
    audio_capture::AudioCapturer,
    audio_session_monitor::{self, AudioMonitorCommand},
    error::{Result, SmtcError},
    smtc_handler::{self},
};

/// 在 `MediaWorker` 内部使用的命令，用于控制其子模块。
///
/// 这个枚举定义了 `MediaWorker` 与其管理的后台任务（如 `smtc_handler`）之间的通信协议。
/// 它将来自外部公共 API (`MediaCommand`) 的意图，转换为内部模块可以理解的具体指令。
#[derive(Debug, Clone)]
pub enum InternalCommand {
    /// 指示 `smtc_handler` 切换到指定的媒体会话。
    SelectSmtcSession(String),
    /// 向 `smtc_handler` 转发一个媒体控制指令（如播放、暂停等）。
    MediaControl(SmtcControlCommand),
    /// 请求 `smtc_handler` 重新获取并广播其当前状态。
    RequestStateUpdate,
    /// 设置 SMTC 元数据的文本转换模式。
    SetTextConversion(TextConversionMode),
    /// 指示 `smtc_handler` 启动或停止其内部的进度模拟计时器。
    SetProgressTimer(bool),
    /// 指示 `smtc_handler` 设置一个偏移量。
    SetProgressOffset(i64),
}

/// 在 `MediaWorker` 内部使用的更新事件，由其子模块发出。
///
/// 这个枚举代表了所有子模块可能产生的事件，`MediaWorker` 的主事件循环会监听这些事件，
/// 然后将它们转换为外部可见的 `MediaUpdate`。
#[derive(Debug, Clone)]
pub enum InternalUpdate {
    /// 由 `smtc_handler` 发出，表示当前曲目信息已更新。
    TrackChanged(NowPlayingInfo),
    /// 由 `smtc_handler` 发出，表示活动的 SMTC 会话已更改。
    ActiveSmtcSessionChanged { pid: Option<u32> },
    /// 由 `audio_session_monitor` 的回调发出，表示被监听的会话状态已发生变化。
    AudioSessionStateChanged {
        session_id: String,
        new_state: AudioSessionState,
    },
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

pub struct MediaWorker {
    command_rx: TokioReceiver<MediaCommand>,
    update_tx: TokioSender<MediaUpdate>,
    smtc_control_tx: Option<TokioSender<InternalCommand>>,
    smtc_update_rx: TokioReceiver<InternalUpdate>,
    diagnostics_rx: Option<TokioReceiver<DiagnosticInfo>>,
    smtc_listener_task_handle: Option<TokioJoinHandle<Result<()>>>,
    smtc_shutdown_tx: Option<TokioSender<()>>,
    audio_monitor_command_tx: Option<TokioSender<AudioMonitorCommand>>,
    audio_monitor_task_handle: Option<TokioJoinHandle<()>>,
    audio_monitor_shutdown_tx: Option<TokioSender<()>>,
    audio_capturer: Option<AudioCapturer>,
    audio_update_rx: Option<TokioReceiver<InternalUpdate>>,
    _shared_player_state: Arc<tokio::sync::Mutex<SharedPlayerState>>,
}

impl MediaWorker {
    async fn run(
        command_rx: TokioReceiver<MediaCommand>,
        update_tx: TokioSender<MediaUpdate>,
    ) -> Result<()> {
        log::info!("[MediaWorker] Worker 正在启动...");

        let (smtc_update_tx, smtc_update_rx) = channel::<InternalUpdate>(32);
        let (smtc_control_tx, smtc_control_rx) = channel::<InternalCommand>(32);
        let (diagnostics_tx, diagnostics_rx) = channel::<DiagnosticInfo>(32);
        let player_state = Arc::new(tokio::sync::Mutex::new(SharedPlayerState::default()));

        let (audio_monitor_command_tx, audio_monitor_command_rx) =
            channel::<AudioMonitorCommand>(8);
        let (audio_monitor_shutdown_tx, audio_monitor_shutdown_rx) = channel::<()>(1);
        let audio_monitor = audio_session_monitor::AudioSessionMonitor::new(
            audio_monitor_command_rx,
            smtc_update_tx.clone(),
        );
        let audio_monitor_handle =
            tokio::task::spawn_local(audio_monitor.run(audio_monitor_shutdown_rx));

        let (shutdown_tx, shutdown_rx) = channel::<()>(1);
        let smtc_listener_handle = tokio::task::spawn_local(smtc_handler::run_smtc_listener(
            smtc_update_tx.clone(),
            smtc_control_rx,
            player_state.clone(),
            shutdown_rx,
            diagnostics_tx,
        ));

        let mut worker_instance = Self {
            command_rx,
            update_tx,
            smtc_control_tx: Some(smtc_control_tx),
            smtc_update_rx,
            diagnostics_rx: Some(diagnostics_rx),
            smtc_listener_task_handle: Some(smtc_listener_handle),
            smtc_shutdown_tx: Some(shutdown_tx),
            audio_monitor_command_tx: Some(audio_monitor_command_tx),
            audio_monitor_task_handle: Some(audio_monitor_handle),
            audio_monitor_shutdown_tx: Some(audio_monitor_shutdown_tx),
            audio_capturer: None,
            audio_update_rx: None,
            _shared_player_state: player_state,
        };

        worker_instance.main_event_loop().await;

        worker_instance.shutdown_all_subsystems().await;

        Ok(())
    }

    async fn main_event_loop(&mut self) {
        loop {
            tokio::select! {
                biased;

                Some(command) = self.command_rx.recv() => {
                    log::trace!("[MediaWorker] 收到外部命令: {command:?}");
                    if matches!(command, MediaCommand::Shutdown) {
                        log::debug!("[MediaWorker] 收到外部关闭命令，准备退出...");
                        break;
                    }
                    self.handle_command_from_app(command).await;
                },

                Some(update) = self.smtc_update_rx.recv() => {
                    self.handle_internal_update(update, "SMTC").await;
                },

                diag_result = async {
                    match self.diagnostics_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                }, if self.diagnostics_rx.is_some() => {
                    if let Some(diag_info) = diag_result
                        && self.update_tx.send(MediaUpdate::Diagnostic(diag_info)).await.is_err() {
                            log::error!("[MediaWorker] 发送 Diagnostic 更新到外部失败。");
                        }
                },

                audio_result = async {
                    match self.audio_update_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                }, if self.audio_update_rx.is_some() => {
                    if let Some(update) = audio_result {
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
        let internal_command = match command {
            MediaCommand::SelectSession(session_id) => {
                Some(InternalCommand::SelectSmtcSession(session_id))
            }
            MediaCommand::Control(smtc_cmd) => Some(InternalCommand::MediaControl(smtc_cmd)),
            MediaCommand::SetTextConversion(mode) => Some(InternalCommand::SetTextConversion(mode)),
            MediaCommand::RequestUpdate => Some(InternalCommand::RequestStateUpdate),
            MediaCommand::SetHighFrequencyProgressUpdates(enabled) => {
                Some(InternalCommand::SetProgressTimer(enabled))
            }
            MediaCommand::SetProgressOffset(offset) => {
                Some(InternalCommand::SetProgressOffset(offset))
            }
            MediaCommand::StartAudioCapture => {
                self.start_audio_capture_internal();
                None
            }
            MediaCommand::StopAudioCapture => {
                self.stop_audio_capture_internal();
                None
            }
            MediaCommand::Shutdown => None,
        };

        if let Some(cmd) = internal_command {
            self.send_internal_command_to_smtc(cmd).await;
        }
    }

    async fn handle_internal_update(&self, internal_update: InternalUpdate, source: &str) {
        let maybe_public_update = match internal_update {
            InternalUpdate::ActiveSmtcSessionChanged { pid } => {
                let command = pid.map_or(AudioMonitorCommand::StopMonitoring, |pid_val| {
                    AudioMonitorCommand::StartMonitoring(pid_val)
                });
                if let Some(tx) = &self.audio_monitor_command_tx
                    && tx.send(command).await.is_err()
                {
                    log::error!("[MediaWorker] 发送命令到音频会话监视器失败。");
                }
                None
            }
            InternalUpdate::AudioSessionStateChanged {
                session_id,
                new_state,
            } => {
                log::debug!("[MediaWorker] 音频会话 '{session_id}' 状态变为: {new_state:?}");

                if new_state == AudioSessionStateExpired {
                    log::info!("[MediaWorker] 音频会话已过期，停止监听。");
                    if let Some(tx) = &self.audio_monitor_command_tx
                        && tx.send(AudioMonitorCommand::StopMonitoring).await.is_err()
                    {
                        log::error!("[MediaWorker] 发送 StopMonitoring 命令失败。");
                    }
                }
                None
            }
            InternalUpdate::TrackChanged(info) => Some(MediaUpdate::TrackChanged(info)),
            InternalUpdate::SmtcSessionListChanged(list) => {
                Some(MediaUpdate::SessionsChanged(list))
            }
            InternalUpdate::AudioDataPacket(bytes) => Some(MediaUpdate::AudioData(bytes)),
            InternalUpdate::AudioSessionVolumeChanged {
                session_id,
                volume,
                is_muted,
            } => Some(MediaUpdate::VolumeChanged {
                session_id,
                volume,
                is_muted,
            }),
            InternalUpdate::SelectedSmtcSessionVanished(session_id) => {
                Some(MediaUpdate::SelectedSessionVanished(session_id))
            }
            InternalUpdate::AudioCaptureError(err) => Some(MediaUpdate::Error(err)),
        };

        if let Some(public_update) = maybe_public_update
            && self.update_tx.send(public_update).await.is_err()
        {
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

    fn start_audio_capture_internal(&mut self) {
        if self.audio_capturer.is_some() {
            log::warn!("[MediaWorker] 音频捕获已在运行，无需重复启动。");
            return;
        }
        log::debug!("[MediaWorker] 正在启动音频捕获...");

        let (audio_update_tx, audio_update_rx) = channel::<InternalUpdate>(256);

        let mut capturer = AudioCapturer::new();
        match capturer.start_capture(audio_update_tx) {
            Ok(()) => {
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
        self.smtc_control_tx.take();
        self.audio_monitor_command_tx.take();

        if let Some(tx) = self.audio_monitor_shutdown_tx.take() {
            let _ = tx.send(()).await;
        }
        if let Some(handle) = self.audio_monitor_task_handle.take()
            && tokio::time::timeout(std::time::Duration::from_secs(1), handle)
                .await
                .is_err()
        {
            log::warn!("[MediaWorker] 音频监视器退出超时，将强制中止。");
        }
        if let Some(tx) = self.smtc_shutdown_tx.take() {
            let _ = tx.send(()).await;
        }
        if let Some(handle) = self.smtc_listener_task_handle.take()
            && tokio::time::timeout(std::time::Duration::from_secs(1), handle)
                .await
                .is_err()
        {
            log::warn!("[MediaWorker] SMTC 监听器退出超时，将强制中止。");
        }
        self.stop_audio_capture_internal();
    }
}

impl Drop for MediaWorker {
    fn drop(&mut self) {
        if self.smtc_control_tx.is_some() {
            log::warn!("[MediaWorker] MediaWorker 实例被意外丢弃 (可能发生 panic)。");
        }
    }
}

pub fn start_media_worker_thread(
    command_rx: TokioReceiver<MediaCommand>,
    update_tx: TokioSender<MediaUpdate>,
) -> Result<thread::JoinHandle<()>> {
    thread::Builder::new()
        .name("media_worker_thread".to_string())
        .spawn(move || {
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

            let _com_guard = match ComGuard::new() {
                Ok(guard) => guard,
                Err(e) => {
                    log::error!("[MediaWorker Thread] COM 初始化失败: {e}，线程无法启动。");
                    return;
                }
            };

            let local_set = LocalSet::new();
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("无法为 MediaWorker 创建 Tokio 运行时");

            local_set.block_on(&rt, async {
                if let Err(e) = MediaWorker::run(command_rx, update_tx).await {
                    log::error!("[MediaWorker Thread] Worker 运行失败: {e}");
                }
            });
        })
        .map_err(|e| SmtcError::WorkerThread(e.to_string()))
}
