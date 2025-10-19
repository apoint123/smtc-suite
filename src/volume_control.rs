use easer::functions::Easing;
use tokio::sync::mpsc::Sender as TokioSender;
use tokio::task::JoinHandle;
use tokio::time::Duration as TokioDuration;
use tokio_util::sync::CancellationToken;
use windows::{
    Win32::{
        Foundation::{CloseHandle, ERROR_NO_MORE_FILES, HANDLE},
        Media::Audio::{
            AudioSessionStateActive, IAudioSessionControl2, IAudioSessionManager2,
            IMMDeviceEnumerator, ISimpleAudioVolume, MMDeviceEnumerator, eConsole, eRender,
        },
        System::{
            Com::{CLSCTX_ALL, CoCreateInstance},
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
                TH32CS_SNAPPROCESS,
            },
        },
    },
    core::{Interface, PCWSTR},
};

use crate::error::{Result, SmtcError};
use crate::worker::InternalUpdate;

const VOLUME_EASING_DURATION_MS: f32 = 250.0;

const VOLUME_EASING_STEPS: u32 = 15;

const VOLUME_EASING_THRESHOLD: f32 = 0.01;

#[allow(clippy::future_not_send)]
async fn volume_easing_task(
    task_id: u64,
    target_vol: f32,
    session_id: String,
    connector_tx: TokioSender<InternalUpdate>,
    cancel_token: CancellationToken,
) {
    let simple_audio_volume = match find_session_control(&session_id)
        .and_then(|session| Ok(session.cast::<ISimpleAudioVolume>()?))
    {
        Ok(volume_control) => volume_control,
        Err(e) => {
            log::warn!(
                "[Easing Task][ID:{task_id}] Unable to obtain audio session control interface: {e}, task aborted."
            );
            return;
        }
    };

    if let Ok(initial_vol) = unsafe { simple_audio_volume.GetMasterVolume() } {
        if (target_vol - initial_vol).abs() < VOLUME_EASING_THRESHOLD {
            let _ = unsafe {
                simple_audio_volume.SetMasterVolume(target_vol, &windows_core::GUID::default())
            };
            if let Ok(final_mute) = unsafe { simple_audio_volume.GetMute() } {
                let _ = connector_tx
                    .send(InternalUpdate::AudioSessionVolumeChanged {
                        session_id,
                        volume: target_vol,
                        is_muted: final_mute.as_bool(),
                    })
                    .await;
            }
            return;
        }

        let animation_duration_ms = VOLUME_EASING_DURATION_MS;
        let steps = VOLUME_EASING_STEPS;
        let step_duration =
            TokioDuration::from_millis((animation_duration_ms / steps as f32) as u64);

        for s in 0..=steps {
            tokio::select! {
                biased;
                () = cancel_token.cancelled() => {
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

                    if unsafe { simple_audio_volume.SetMasterVolume(current_vol, &windows_core::GUID::default()) }
                        .is_err()
                    {
                        log::warn!("[Easing Task][ID:{task_id}] Failed to set volume, task aborted.");
                        break;
                    }
                }
            }
        }

        let _ = unsafe {
            simple_audio_volume
                .SetMasterVolume(target_vol.clamp(0.0, 1.0), &windows_core::GUID::default())
        };

        if let (Ok(final_vol), Ok(final_mute)) = unsafe {
            (
                simple_audio_volume.GetMasterVolume(),
                simple_audio_volume.GetMute(),
            )
        } {
            let _ = connector_tx
                .send(InternalUpdate::AudioSessionVolumeChanged {
                    session_id,
                    volume: final_vol,
                    is_muted: final_mute.as_bool(),
                })
                .await;
        }
    } else {
        log::warn!(
            "[Easing Task][ID:{task_id}] Unable to obtain the initial volume. Task aborted."
        );
    }
}

pub fn spawn_volume_easing_task(
    task_id: u64,
    level: f32,
    session_id: String,
    connector_update_tx: TokioSender<InternalUpdate>,
) -> (JoinHandle<()>, CancellationToken) {
    let cancel_token = CancellationToken::new();
    let handle = tokio::task::spawn_local(volume_easing_task(
        task_id,
        level,
        session_id,
        connector_update_tx,
        cancel_token.clone(),
    ));
    (handle, cancel_token)
}

fn find_session_control(identifier: &str) -> Result<IAudioSessionControl2> {
    let target_pid = get_pid_from_identifier(identifier);
    let uwp_parts = parse_uwp_identifier(identifier);

    find_audio_session_with(|session| {
        if let Some(pid) = target_pid
            && let Ok(session_pid) = unsafe { session.GetProcessId() }
            && session_pid > 0
            && session_pid == pid
        {
            return true;
        }

        if let Some((name_part, publisher_id)) = &uwp_parts {
            if let Ok(hstring) = unsafe { session.GetSessionIdentifier() }
                && let Ok(session_id_str) = unsafe { hstring.to_string() }
            {
                let session_id_lower = session_id_str.to_lowercase();
                if session_id_lower.contains(name_part) && session_id_lower.contains(publisher_id) {
                    return true;
                }
            }

            if let Ok(pwstr) = unsafe { session.GetIconPath() }
                && !pwstr.is_null()
                && let Ok(icon_path_str) = unsafe { pwstr.to_string() }
            {
                let icon_path_lower = icon_path_str.to_lowercase();
                if icon_path_lower.contains(name_part) && icon_path_lower.contains(publisher_id) {
                    return true;
                }
            }
        }

        false
    })
    .map_err(|_| {
        SmtcError::VolumeControl(format!(
            "No matching active audio session found for identifier '{identifier}'."
        ))
    })
}

fn parse_uwp_identifier(identifier: &str) -> Option<(String, String)> {
    if identifier.contains('!') {
        identifier
            .split('!')
            .next()
            .and_then(|pfn| pfn.rsplit_once('_'))
            .map(|(name, publisher)| (name.to_lowercase(), publisher.to_lowercase()))
    } else {
        None
    }
}

pub fn get_pid_from_identifier(identifier: &str) -> Option<u32> {
    if let Some(pid) = get_pid_from_executable_name(identifier) {
        log::trace!(
            "[Volume Control] Found the PID by the executable file name '{identifier}': {pid}"
        );
        return Some(pid);
    }

    if !identifier.to_lowercase().ends_with(".exe") {
        let exe_name = format!("{identifier}.exe");
        if let Some(pid) = get_pid_from_executable_name(&exe_name) {
            log::trace!("[Volume Control] Found the PID by appending .exe '{exe_name}': {pid}");
            return Some(pid);
        }
    }

    if identifier.contains('!')
        && let Some(derived_exe_name) = derive_executable_name_from_aumid(identifier)
        && let Some(pid) = get_pid_from_executable_name(&derived_exe_name)
    {
        log::debug!(
            "[Volume Control] Found PID by executable file name '{}' inferred from AUMID: {pid}",
            &derived_exe_name,
        );
        return Some(pid);
    }

    log::warn!("[Volume Control] Unable to resolve PID from identifier '{identifier}'.");
    None
}

fn derive_executable_name_from_aumid(aumid: &str) -> Option<String> {
    aumid
        .split('!')
        .next()
        .and_then(|pfn_part| pfn_part.split('_').next())
        .and_then(|name_part| name_part.rsplit('.').next())
        .map(|app_name| {
            let base_name = app_name
                .trim_end_matches("Win")
                .trim_end_matches("Uwp")
                .trim_end_matches("Desktop");
            format!("{base_name}.exe")
        })
}

fn get_pid_from_executable_name(executable_name: &str) -> Option<u32> {
    let snapshot_handle = match unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) } {
        Ok(handle) if !handle.is_invalid() => handle,
        _ => return None,
    };

    struct HandleGuard(HANDLE);
    impl Drop for HandleGuard {
        fn drop(&mut self) {
            if !self.0.is_invalid() {
                unsafe { CloseHandle(self.0).ok() };
            }
        }
    }
    let _guard = HandleGuard(snapshot_handle);

    let mut process_entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };

    if unsafe { Process32FirstW(snapshot_handle, &raw mut process_entry) }.is_err() {
        return None;
    }

    loop {
        let current_exe_name_str = unsafe {
            PCWSTR::from_raw(process_entry.szExeFile.as_ptr())
                .to_string()
                .unwrap_or_default()
        };

        if current_exe_name_str.eq_ignore_ascii_case(executable_name) {
            return Some(process_entry.th32ProcessID);
        }

        if unsafe { Process32NextW(snapshot_handle, &raw mut process_entry) }.is_err()
            && windows::core::Error::from_thread().code() == ERROR_NO_MORE_FILES.to_hresult()
        {
            break;
        }
    }
    None
}

fn find_audio_session_with<F>(mut predicate: F) -> Result<IAudioSessionControl2>
where
    F: FnMut(&IAudioSessionControl2) -> bool,
{
    let sessions = unsafe { get_all_active_audio_sessions() }?;

    for session in sessions {
        if predicate(&session) {
            return Ok(session);
        }
    }

    Err(SmtcError::VolumeControl(
        "No matches were found in any active sessions.".to_string(),
    ))
}

unsafe fn get_all_active_audio_sessions() -> Result<Vec<IAudioSessionControl2>> {
    unsafe {
        let device_enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let default_device = device_enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
        let session_manager: IAudioSessionManager2 = default_device.Activate(CLSCTX_ALL, None)?;
        let session_enumerator = session_manager.GetSessionEnumerator()?;
        let count = session_enumerator.GetCount()?;
        let mut result_sessions = Vec::with_capacity(count as usize);

        for i in 0..count {
            if let Ok(session_control) = session_enumerator.GetSession(i)
                && let Ok(session_control2) = session_control.cast::<IAudioSessionControl2>()
                && session_control2.GetState() == Ok(AudioSessionStateActive)
            {
                result_sessions.push(session_control2);
            }
        }
        Ok(result_sessions)
    }
}
