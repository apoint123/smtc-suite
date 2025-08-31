use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use windows::{
    Win32::{
        Foundation::{CloseHandle, ERROR_NO_MORE_FILES, HANDLE},
        Media::Audio::{
            AudioSessionStateActive, IAudioSessionControl2, IAudioSessionEnumerator,
            IAudioSessionManager2, IMMDeviceEnumerator, ISimpleAudioVolume, MMDeviceEnumerator,
            eConsole, eRender,
        },
        System::{
            Com::{CLSCTX_ALL, CoCreateInstance},
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
                TH32CS_SNAPPROCESS,
            },
        },
    },
    core::Interface,
};

use crate::error::{Result, SmtcError};

/// 通过应用程序的标识符（AUMID 或可执行文件名）获取其音量和静音状态。
///
/// # 参数
/// * `identifier`: 目标应用的标识符字符串。
///   - 对于 UWP 应用，应为 AUMID (e.g., "`Microsoft.ZuneMusic_8wekyb3d8bbwe!Microsoft.ZuneMusic`")。
///   - 对于 Win32 应用，应为可执行文件名 (e.g., "Spotify.exe")。
///
/// # 返回
/// - `Ok((volume, is_muted))`: 成功时返回一个元组，包含音量（0.0-1.0）和静音状态（布尔值）。
/// - `Err(SmtcError)`: 如果找不到匹配的活动音频会话，或发生其他 Windows API 错误。
pub fn get_volume_for_identifier(identifier: &str) -> Result<(f32, bool)> {
    log::debug!("[音量控制] 尝试获取标识符 '{identifier}' 的音量。");
    let session_control = find_session_control_for_identifier(identifier)?;
    let simple_audio_volume: ISimpleAudioVolume = session_control.cast()?;
    let volume = unsafe { simple_audio_volume.GetMasterVolume()? };
    let muted = unsafe { simple_audio_volume.GetMute()?.as_bool() };
    Ok((volume, muted))
}

/// 通过应用程序的标识符设置其音量和/或静音状态。
///
/// # 参数
/// * `identifier`: 目标应用的标识符字符串。
/// * `volume_level`: 可选的目标音量级别 (0.0 到 1.0)。如果为 `None`，则不改变音量。
/// * `mute`: 可选的静音状态。如果为 `None`，则不改变静音状态。
pub fn set_volume_for_identifier(
    identifier: &str,
    volume_level: Option<f32>,
    mute: Option<bool>,
) -> Result<()> {
    // 如果没有提供任何操作，则直接返回。
    if volume_level.is_none() && mute.is_none() {
        return Ok(());
    }

    log::debug!(
        "[音量控制] 尝试为标识符 '{identifier}' 设置音量: {volume_level:?}, 静音: {mute:?}"
    );
    let session_control = find_session_control_for_identifier(identifier)?;
    let simple_audio_volume: ISimpleAudioVolume = session_control.cast()?;

    // SAFETY: 所有 Set* 调用都是安全的 COM FFI 调用。
    unsafe {
        if let Some(vol) = volume_level {
            // GUID::default() 表示此音量变化事件的上下文为通用，没有特定的事件源。
            simple_audio_volume
                .SetMasterVolume(vol.clamp(0.0, 1.0), &windows_core::GUID::default())?;
        }
        if let Some(m) = mute {
            simple_audio_volume.SetMute(m, &windows_core::GUID::default())?;
        }
    }
    Ok(())
}

/// 根据给定的应用标识符，在系统中查找匹配的、活动的音频会话控制器。
fn find_session_control_for_identifier(identifier: &str) -> Result<IAudioSessionControl2> {
    // 预先获取 PID，作为后备匹配策略使用。
    let target_pid = get_pid_from_identifier(identifier);

    // 如果是 UWP 应用，预先分解出其“应用名”和“发行商ID”部分。
    let uwp_name_and_publisher_id: Option<(&str, &str)> = if identifier.contains('!') {
        identifier
            .split('!')
            .next()
            .and_then(|pfn| pfn.rsplit_once('_'))
    } else {
        None
    };

    unsafe {
        let device_enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let default_device = device_enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
        let session_manager: IAudioSessionManager2 = default_device.Activate(CLSCTX_ALL, None)?;
        let session_enumerator = session_manager.GetSessionEnumerator()?;
        let count = session_enumerator.GetCount()?;

        let mut all_sessions_for_debug = Vec::new();

        // 遍历系统中所有的音频会话
        for i in 0..count {
            if let Ok(session_control) = session_enumerator.GetSession(i)
                && let Ok(session_control2) = session_control.cast::<IAudioSessionControl2>()
            {
                // --- UWP 应用匹配策略 ---
                if let Some((name_part, publisher_id)) = uwp_name_and_publisher_id {
                    let name_part_lower = name_part.to_lowercase();
                    let publisher_id_lower = publisher_id.to_lowercase();

                    // 策略 1A: 检查 SessionIdentifier (针对 Apple Music 等)
                    if let Ok(hstring) = session_control2.GetSessionIdentifier()
                        && let Ok(session_id_str) = hstring.to_string()
                    {
                        let session_id_lower = session_id_str.to_lowercase();
                        if session_id_lower.contains(&name_part_lower)
                            && session_id_lower.contains(&publisher_id_lower)
                            && let Ok(state) = session_control2.GetState()
                            && state == AudioSessionStateActive
                        {
                            log::info!(
                                "[音量控制] 通过匹配 SessionIdentifier 中的组件找到UWP音频会话。"
                            );
                            return Ok(session_control2);
                        }
                    }

                    // 策略 1B: 检查 IconPath (针对某些特殊UWP应用)
                    if let Ok(pwstr) = session_control2.GetIconPath()
                        && !pwstr.is_null()
                        && let Ok(icon_path_str) = pwstr.to_string()
                    {
                        let icon_path_lower = icon_path_str.to_lowercase();
                        if icon_path_lower.contains(&name_part_lower)
                            && icon_path_lower.contains(&publisher_id_lower)
                            && let Ok(state) = session_control2.GetState()
                            && state == AudioSessionStateActive
                        {
                            log::info!("[音量控制] 通过匹配 IconPath 中的组件找到UWP音频会话。");
                            return Ok(session_control2);
                        }
                    }
                }

                // --- 策略 2 (Win32 或后备): 按 PID 匹配 ---
                if let Some(pid) = target_pid
                    && let Ok(session_pid) = session_control2.GetProcessId()
                    && session_pid > 0
                    && session_pid == pid
                    && let Ok(state) = session_control2.GetState()
                    && state == AudioSessionStateActive
                {
                    log::info!("[音量控制] 通过 PID {pid} 找到匹配的音频会话。");
                    return Ok(session_control2);
                }

                // --- 调试信息收集 ---
                // 如果日志级别足够高，则收集所有会话信息，以便在最终失败时打印。
                if log::max_level() >= log::LevelFilter::Error
                    && let Ok(state) = session_control2.GetState()
                {
                    let pid = session_control2.GetProcessId().unwrap_or(0);
                    let display_name = match session_control2.GetDisplayName() {
                        Ok(s) if !s.is_null() => s
                            .to_string()
                            .unwrap_or_else(|_| "<Invalid UTF-16>".to_string()),
                        _ => String::new(),
                    };
                    let session_id = session_control2
                        .GetSessionIdentifier()
                        .map_or_else(|_| String::new(), |s| s.to_string().unwrap_or_default());
                    let icon_path = match session_control2.GetIconPath() {
                        Ok(s) if !s.is_null() => s
                            .to_string()
                            .unwrap_or_else(|_| "<Invalid UTF-16>".to_string()),
                        _ => String::new(),
                    };

                    all_sessions_for_debug.push(format!(
                                "  > [会话 {i}] 状态: {state:?}, PID: {pid}, 显示名称: '{display_name}', 图标路径: '{icon_path}', 会话ID: '{session_id}'"
                            ));
                }
            }
        }

        // 如果循环结束仍未找到匹配的会话，则打印所有收集到的会话信息。
        log::error!("[音量控制] 未能为标识符 '{identifier}' 找到匹配的活动音频会话。");
        if !all_sessions_for_debug.is_empty() {
            log::trace!("[音量控制] 以下是系统中所有（包括非活动）音频会话的快照:");
            for session_info in all_sessions_for_debug {
                log::trace!("{session_info}");
            }
        }
    }

    Err(SmtcError::VolumeControl(format!(
        "在所有会话中都未找到与标识符 '{identifier}' 匹配的活动项。"
    )))
}

/// 从给定的标识符（可执行文件名或 AUMID）动态地获取进程ID (PID)。
///
/// 此函数同样采用多策略来提高成功率。
pub fn get_pid_from_identifier(identifier: &str) -> Option<u32> {
    log::debug!("[音量控制] 尝试从标识符 '{identifier}' 获取 PID。");

    // 策略 1: 直接将标识符作为可执行文件名进行匹配。
    if let Some(pid) = get_pid_from_executable_name(identifier) {
        log::trace!("[音量控制] 通过可执行文件名 '{identifier}' 直接找到 PID: {pid}");
        return Some(pid);
    }

    // 如果标识符包含 '!'，则认为它是一个 AUMID，并尝试UWP相关策略。
    if identifier.contains('!') {
        // 策略 2: 从 AUMID 启发式地推断出可能的可执行文件名并进行匹配。
        if let Some(derived_exe_name) = derive_executable_name_from_aumid(identifier) {
            log::debug!(
                "[音量控制] 标识符是 AUMID，已推断出候选可执行文件名: '{}'",
                &derived_exe_name
            );
            if let Some(pid) = get_pid_from_executable_name(&derived_exe_name) {
                log::debug!(
                    "[音量控制] 通过从 AUMID 推断出的可执行文件名 '{}' 找到 PID: {}",
                    &derived_exe_name,
                    pid
                );
                return Some(pid);
            }
        }

        // 策略 3 (后备): 通过遍历所有音频会话的 IconPath 来匹配。
        // 这是一种成本较高但有时很有效的方法。
        log::debug!("[音量控制] 推断 .exe 名称失败，回退到通过音频会话查找。");
        if let Some(pfn_from_aumid) = identifier.split('!').next() {
            match find_pid_for_aumid_via_audio_sessions(pfn_from_aumid) {
                Ok(Some(pid)) => {
                    log::debug!("[音量控制] 通过音频会话为 AUMID '{identifier}' 找到 PID: {pid}");
                    return Some(pid);
                }
                Ok(None) => {
                    log::warn!(
                        "[音量控制] 无法通过音频会话为 AUMID '{identifier}' 找到匹配的 PID。"
                    );
                }
                Err(e) => {
                    log::error!(
                        "[音量控制] 通过音频会话查找 AUMID '{identifier}' 的 PID 时出错: {e}"
                    );
                }
            }
        }
    }

    log::warn!("[音量控制] 无法从标识符 '{identifier}' 解析 PID。");
    None
}

/// 从 AUMID 中启发式地推断出可能的可执行文件名。
///
/// 这种方法比依赖音频会话的 `IconPath` 更可靠，特别是对于安装在系统目录的应用。
///
/// # 示例
/// - `AppleInc.AppleMusicWin_nzyj5cx40ttqa!App` -> `AppleMusic.exe`
/// - `Microsoft.ZuneMusic_8wekyb3d8bbwe!Microsoft.ZuneMusic` -> `ZuneMusic.exe`
fn derive_executable_name_from_aumid(aumid: &str) -> Option<String> {
    aumid
        .split('!')
        .next() // 1. 取 '!' 前的部分: "AppleInc.AppleMusicWin_nzyj5cx40ttqa"
        .and_then(|pfn_part| pfn_part.split('_').next()) // 2. 取 '_' 前的部分: "AppleInc.AppleMusicWin"
        .and_then(|name_part| name_part.rsplit('.').next()) // 3. 取最后一个 '.' 之后的部分: "AppleMusicWin"
        .map(|app_name| {
            // 4. 移除常见的 "Win" 或 "Uwp" 后缀
            let base_name = app_name
                .trim_end_matches("Win")
                .trim_end_matches("Uwp")
                .trim_end_matches("Desktop"); // 也移除 Desktop 后缀
            // 5. 拼接成 .exe 文件名
            format!("{base_name}.exe")
        })
}

/// 通过可执行文件名获取进程 ID (PID)。
///
/// 使用 Windows Tool Help Library (`CreateToolhelp32Snapshot`) 遍历系统中的所有进程，
/// 不区分大小写地匹配其可执行文件名。
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
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };

    if unsafe { Process32FirstW(snapshot_handle, &raw mut process_entry) }.is_err() {
        return None;
    }

    loop {
        // 从宽字符数组转换为 Rust 的 OsString，然后是 &str。
        let current_exe_name_wide = &process_entry.szExeFile;
        let len = current_exe_name_wide
            .iter()
            .take_while(|&&c| c != 0)
            .count();
        let current_exe_name_os = OsString::from_wide(&current_exe_name_wide[..len]);

        if let Some(current_exe_name_str) = current_exe_name_os.to_str()
            && current_exe_name_str.eq_ignore_ascii_case(executable_name)
        {
            return Some(process_entry.th32ProcessID);
        }

        // SAFETY: Process32NextW 用于移动到下一个进程。
        if unsafe { Process32NextW(snapshot_handle, &raw mut process_entry) }.is_err() {
            // 如果错误是 ERROR_NO_MORE_FILES，说明已遍历完所有进程，这是正常情况。
            if windows::core::Error::from_win32().code() == ERROR_NO_MORE_FILES.to_hresult() {
                break;
            }
        }
    }
    None
}

/// 尝试通过分析活动音频会话的元数据，为给定的 `PackageFamilyName` (源自 AUMID) 找到 PID。
///
/// 这是针对 UWP 应用等使用 AUMID 的场景。它会遍历所有音频会话，
/// 检查其 `IconPath`，并尝试从中启发式地提取 PFN (Package Family Name) 进行匹配。
fn find_pid_for_aumid_via_audio_sessions(target_pfn_from_aumid: &str) -> Result<Option<u32>> {
    // SAFETY: COM 调用
    unsafe {
        let device_enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let default_device = device_enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
        let session_manager: IAudioSessionManager2 = default_device.Activate(CLSCTX_ALL, None)?;
        let session_enumerator: IAudioSessionEnumerator = session_manager.GetSessionEnumerator()?;
        let count = session_enumerator.GetCount()?;

        for i in 0..count {
            if let Ok(session_control) = session_enumerator.GetSession(i)
                && let Ok(session_control2) = session_control.cast::<IAudioSessionControl2>()
            {
                // 仅处理活动的音频会话
                if let Ok(current_state) = session_control2.GetState()
                    && current_state == AudioSessionStateActive
                    && let Ok(pid) = session_control2.GetProcessId()
                    && pid > 0
                    && let Ok(icon_path_pwstr) = session_control2.GetIconPath()
                    && !icon_path_pwstr.is_null()
                    && let Ok(icon_path_str) = icon_path_pwstr.to_string()
                {
                    // 检查 IconPath 是否包含目标 PFN
                    if icon_path_str
                        .to_lowercase()
                        .contains(&target_pfn_from_aumid.to_lowercase())
                    {
                        log::debug!("[音量控制] 通过 IconPath 找到匹配的 PID: {pid}");
                        return Ok(Some(pid));
                    }
                }
            }
        }
    }
    Ok(None) // 未找到匹配的 PID
}
