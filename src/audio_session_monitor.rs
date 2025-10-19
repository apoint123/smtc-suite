#![allow(clippy::ptr_as_ptr)]
#![allow(clippy::borrow_as_ptr)]
#![allow(clippy::ref_as_ptr)]
#![allow(clippy::inline_always)]

use std::sync::{Arc, Mutex};

use tokio::sync::mpsc::{Receiver as TokioReceiver, Sender as TokioSender};
use windows::{
    Win32::{
        Media::Audio::{
            AudioSessionState, AudioSessionStateExpired, IAudioSessionControl,
            IAudioSessionControl2, IAudioSessionEvents, IAudioSessionEvents_Impl,
            IAudioSessionManager2, IMMDeviceEnumerator, ISimpleAudioVolume, MMDeviceEnumerator,
            eConsole, eRender,
        },
        System::Com::{CLSCTX_ALL, CoCreateInstance},
    },
    core::{GUID, Interface, Result as WinResult, implement},
};
use windows_core::BOOL;

use crate::worker::InternalUpdate;

#[derive(Debug)]
pub enum AudioMonitorCommand {
    StartMonitoring(u32),
    StopMonitoring,
}

type NotifierTx = Arc<Mutex<Option<TokioSender<InternalUpdate>>>>;

#[implement(IAudioSessionEvents)]
struct VolumeChangeNotifier {
    tx: NotifierTx,
    session_id: String,
}

#[allow(non_snake_case)]
impl IAudioSessionEvents_Impl for VolumeChangeNotifier_Impl {
    fn OnDisplayNameChanged(
        &self,
        _: &windows::core::PCWSTR,
        _: *const GUID,
    ) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnIconPathChanged(
        &self,
        _: &windows::core::PCWSTR,
        _: *const GUID,
    ) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnSimpleVolumeChanged(
        &self,
        fNewVolume: f32,
        bNewMute: BOOL,
        _EventContext: *const GUID,
    ) -> windows::core::Result<()> {
        if let Ok(guard) = self.tx.lock()
            && let Some(tx) = guard.as_ref()
        {
            let update = InternalUpdate::AudioSessionVolumeChanged {
                session_id: self.session_id.clone(),
                volume: fNewVolume,
                is_muted: bNewMute.as_bool(),
            };
            if let Err(e) = tx.try_send(update) {
                log::warn!("[Volume Monitor] Failed to send volume update: {e}");
            }
        }
        Ok(())
    }
    fn OnChannelVolumeChanged(
        &self,
        _: u32,
        _: *const f32,
        _: u32,
        _: *const GUID,
    ) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnGroupingParamChanged(&self, _: *const GUID, _: *const GUID) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnStateChanged(&self, new_state: AudioSessionState) -> windows::core::Result<()> {
        log::debug!(
            "[Volume Monitor] Session '{}' status changed to: {new_state:?}",
            self.session_id,
        );
        if let Ok(guard) = self.tx.lock()
            && let Some(tx) = guard.as_ref()
        {
            let update = InternalUpdate::AudioSessionStateChanged {
                session_id: self.session_id.clone(),
                new_state,
            };
            if tx.try_send(update).is_err() {
                log::warn!("[Volume Monitor] Failed to send status update.");
            }
        }
        Ok(())
    }
    fn OnSessionDisconnected(
        &self,
        disconnect_reason: windows::Win32::Media::Audio::AudioSessionDisconnectReason,
    ) -> windows::core::Result<()> {
        log::debug!(
            "[Volume Monitor] Session '{}' disconnected ({disconnect_reason:?})",
            self.session_id,
        );
        if let Ok(guard) = self.tx.lock()
            && let Some(tx) = guard.as_ref()
        {
            let update = InternalUpdate::AudioSessionStateChanged {
                session_id: self.session_id.clone(),
                new_state: AudioSessionStateExpired,
            };
            if tx.try_send(update).is_err() {
                log::warn!("[Volume Monitor] Failed to send disconnect event.");
            }
        }
        Ok(())
    }
}

pub struct AudioSessionMonitor {
    command_rx: TokioReceiver<AudioMonitorCommand>,
    update_tx: TokioSender<InternalUpdate>,
    active_session_control: Option<IAudioSessionControl>,
    active_notifier: Option<IAudioSessionEvents>,
}

impl AudioSessionMonitor {
    pub const fn new(
        command_rx: TokioReceiver<AudioMonitorCommand>,
        update_tx: TokioSender<InternalUpdate>,
    ) -> Self {
        Self {
            command_rx,
            update_tx,
            active_session_control: None,
            active_notifier: None,
        }
    }

    #[allow(clippy::future_not_send)]
    pub async fn run(mut self, mut shutdown_rx: TokioReceiver<()>) {
        loop {
            tokio::select! {
                biased;

                _ = shutdown_rx.recv() => {
                    break;
                },

                Some(command) = self.command_rx.recv() => {
                    if let Err(e) = self.handle_command(&command) {
                        log::error!("[Volume Monitor] Failed to process command: {e:?}");
                    }
                },

                else => {
                    break;
                }
            }
        }
        if let Err(e) = self.stop_monitoring_internal() {
            log::warn!("[Volume Monitor] Logout callback failed when exiting: {e:?}");
        }
    }

    fn handle_command(&mut self, command: &AudioMonitorCommand) -> WinResult<()> {
        match command {
            AudioMonitorCommand::StartMonitoring(pid) => {
                self.stop_monitoring_internal()?;
                unsafe {
                    let device_enumerator: IMMDeviceEnumerator =
                        CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
                    let default_device =
                        device_enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
                    let session_manager: IAudioSessionManager2 =
                        default_device.Activate(CLSCTX_ALL, None)?;
                    let session_enumerator = session_manager.GetSessionEnumerator()?;
                    let count = session_enumerator.GetCount()?;

                    for i in 0..count {
                        let session_control: IAudioSessionControl =
                            session_enumerator.GetSession(i)?;
                        let session_control2: IAudioSessionControl2 = session_control.cast()?;

                        if session_control2.GetProcessId()? == *pid {
                            let simple_audio_volume: ISimpleAudioVolume = session_control.cast()?;
                            let initial_volume = simple_audio_volume.GetMasterVolume()?;
                            let is_muted = simple_audio_volume.GetMute()?.as_bool();
                            let session_id =
                                session_control2.GetSessionIdentifier()?.to_string()?;

                            let initial_update = InternalUpdate::AudioSessionVolumeChanged {
                                session_id: session_id.clone(),
                                volume: initial_volume,
                                is_muted,
                            };

                            if self.update_tx.try_send(initial_update).is_err() {
                                log::warn!(
                                    "[Volume Monitor] Failed to send initial volume update."
                                );
                            }

                            let notifier = VolumeChangeNotifier {
                                tx: Arc::new(Mutex::new(Some(self.update_tx.clone()))),
                                session_id,
                            }
                            .into();

                            session_control.RegisterAudioSessionNotification(&notifier)?;

                            self.active_session_control = Some(session_control);
                            self.active_notifier = Some(notifier);
                            return Ok(());
                        }
                    }
                }
            }
            AudioMonitorCommand::StopMonitoring => {
                self.stop_monitoring_internal()?;
            }
        }
        Ok(())
    }

    fn stop_monitoring_internal(&mut self) -> WinResult<()> {
        if let (Some(control), Some(notifier)) = (
            self.active_session_control.take(),
            self.active_notifier.take(),
        ) {
            unsafe { control.UnregisterAudioSessionNotification(&notifier)? };
        }
        Ok(())
    }
}
