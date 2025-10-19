use std::{sync::Arc, thread, time::Duration};

use tokio::{
    sync::mpsc::{Receiver as TokioReceiver, Sender as TokioSender, channel},
    task::JoinHandle as TokioJoinHandle,
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

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);

/// Internal commands used within the `MediaWorker` to control its sub-modules.
#[derive(Debug, Clone)]
pub enum InternalCommand {
    /// Instructs `smtc_handler` to switch to the specified media session.
    SelectSmtcSession(String),
    /// Send a control command (e.g., play, pause) to `smtc_handler`.
    MediaControl(SmtcControlCommand),
    /// Requests `smtc_handler` to re-fetch and broadcast its current state.
    RequestStateUpdate,
    /// Sets the text conversion mode.
    SetTextConversion(TextConversionMode),
    /// Instructs `smtc_handler` to start or stop its progress timer.
    SetProgressTimer(bool),
    /// Instructs `smtc_handler` to set a progress offset.
    SetProgressOffset(i64),
    /// Instructs `smtc_handler` to enable or disable Apple Music optimizations.
    SetAppleMusicOptimization(bool),
}

/// Internal update events emitted by sub-modules within the `MediaWorker`.
///
/// `MediaWorker` listens for these events and translates them into externally
/// `MediaUpdate`s.
#[derive(Debug, Clone)]
pub enum InternalUpdate {
    /// Emitted by `smtc_handler`, indicating that the current track information
    /// has been updated.
    TrackChanged(Box<NowPlayingInfo>),
    /// Emitted by `smtc_handler`, indicating that the active SMTC session has
    /// changed.
    ActiveSmtcSessionChanged { pid: Option<u32> },
    /// Emitted by a callback from `audio_session_monitor`, indicating a change
    /// in the monitored session's state.
    AudioSessionStateChanged {
        session_id: String,
        new_state: AudioSessionState,
    },
    /// Emitted by `smtc_handler`, indicating that the list of available SMTC
    /// sessions has changed.
    SmtcSessionListChanged(Vec<SmtcSessionInfo>),
    /// Emitted by `smtc_handler`, indicating that the previously selected
    /// session has vanished.
    SelectedSmtcSessionVanished(String),
    /// Emitted by `audio_capturer`, containing a captured audio data packet.
    AudioDataPacket(Vec<u8>),
    /// Emitted by `audio_capturer`, representing an error.
    AudioCaptureError(String),
    /// Emitted by `volume_control` (forwarded via `smtc_handler`), indicating a
    /// change in an application's volume.
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
                    log::trace!("[MediaWorker] Received external command: {command:?}");
                    if matches!(command, MediaCommand::Shutdown) {
                        log::debug!("[MediaWorker] Received external shutdown command, preparing to exit...");
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
                            log::error!("[MediaWorker] Failed to send Diagnostic update externally.");
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
                        log::warn!("[MediaWorker] Audio capture update channel disconnected (thread may have exited).");
                        let _ = self.update_tx.send(MediaUpdate::Error("Audio capture failure".to_string())).await;
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
            MediaCommand::SetAppleMusicOptimization(enabled) => {
                Some(InternalCommand::SetAppleMusicOptimization(enabled))
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
                    log::error!("[MediaWorker] Failed to send command to audio session monitor.");
                }
                None
            }
            InternalUpdate::AudioSessionStateChanged {
                session_id,
                new_state,
            } => {
                log::debug!(
                    "[MediaWorker] Audio session '{session_id}' state changed to: {new_state:?}"
                );

                if new_state == AudioSessionStateExpired {
                    log::info!("[MediaWorker] Audio session has expired, stopping monitoring.");
                    if let Some(tx) = &self.audio_monitor_command_tx
                        && tx.send(AudioMonitorCommand::StopMonitoring).await.is_err()
                    {
                        log::error!("[MediaWorker] Failed to send StopMonitoring command.");
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
            log::error!("[MediaWorker] Failed to send update (from {source}).");
        }
    }

    async fn send_internal_command_to_smtc(&self, command: InternalCommand) {
        if let Some(sender) = &self.smtc_control_tx {
            if sender.send(command).await.is_err() {
                log::error!("[MediaWorker] Failed to send command to SMTC handler.");
            }
        } else {
            log::error!("[MediaWorker] SMTC control channel is invalid, cannot send command.");
        }
    }

    fn start_audio_capture_internal(&mut self) {
        if self.audio_capturer.is_some() {
            log::warn!("[MediaWorker] Audio capture is already running, skipping start request.");
            return;
        }
        log::debug!("[MediaWorker] Starting audio capture...");

        let (audio_update_tx, audio_update_rx) = channel::<InternalUpdate>(256);

        let mut capturer = AudioCapturer::new();
        match capturer.start_capture(audio_update_tx) {
            Ok(()) => {
                self.audio_capturer = Some(capturer);
                self.audio_update_rx = Some(audio_update_rx);
            }
            Err(e) => {
                log::error!("[MediaWorker] Failed to start audio capture: {e}");
                self.audio_capturer = None;
                self.audio_update_rx = None;
            }
        }
    }

    fn stop_audio_capture_internal(&mut self) {
        if let Some(mut capturer) = self.audio_capturer.take() {
            log::debug!("[MediaWorker] Stopping audio capture...");
            capturer.stop_capture();
        }
        if self.audio_update_rx.take().is_some() {
            log::debug!("[MediaWorker] Audio update channel cleaned up.");
        }
    }

    async fn shutdown_all_subsystems(&mut self) {
        log::info!("[MediaWorker] Shutting down all subsystems...");
        self.smtc_control_tx.take();
        self.audio_monitor_command_tx.take();

        let audio_monitor_shutdown = async {
            if let Some(tx) = self.audio_monitor_shutdown_tx.take() {
                let _ = tx.send(()).await;
            }
            if let Some(mut handle) = self.audio_monitor_task_handle.take()
                && tokio::time::timeout(SHUTDOWN_TIMEOUT, &mut handle)
                    .await
                    .is_err()
            {
                log::warn!("[MediaWorker] Audio monitor shutdown timed out, aborting task.");
                handle.abort();
            }
        };

        let smtc_listener_shutdown = async {
            if let Some(tx) = self.smtc_shutdown_tx.take() {
                let _ = tx.send(()).await;
            }
            if let Some(mut handle) = self.smtc_listener_task_handle.take()
                && tokio::time::timeout(SHUTDOWN_TIMEOUT, &mut handle)
                    .await
                    .is_err()
            {
                log::warn!("[MediaWorker] SMTC listener shutdown timed out, aborting task.");
                handle.abort();
            }
        };

        tokio::join!(audio_monitor_shutdown, smtc_listener_shutdown);

        self.stop_audio_capture_internal();
    }
}

impl Drop for MediaWorker {
    fn drop(&mut self) {
        if self.smtc_control_tx.is_some() {
            log::warn!(
                "[MediaWorker] MediaWorker instance was dropped unexpectedly (panic may have occurred)."
            );
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
                    log::error!(
                        "[MediaWorker Thread] COM initialization failed: {e}, thread cannot start."
                    );
                    return;
                }
            };

            let local_set = LocalSet::new();
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("Failed to create Tokio runtime for MediaWorker");

            local_set.block_on(&rt, async {
                if let Err(e) = MediaWorker::run(command_rx, update_tx).await {
                    log::error!("[MediaWorker Thread] Worker failed to run: {e}");
                }
            });
        })
        .map_err(|e| SmtcError::WorkerThread(e.to_string()))
}
