// src/ffi.rs

use crate::{
    MediaController, MediaManager, TextConversionMode,
    api::{
        CControlCommandType, CRepeatMode, CSmtcControlCommand, CTextConversionMode, MediaCommand,
        MediaUpdate, NowPlayingInfo, RepeatMode, SmtcControlCommand, SmtcSessionInfo,
    },
};
use crossbeam_channel::{Receiver as CrossbeamReceiver, select};
use std::{
    ffi::{CStr, CString, c_char, c_void},
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

// ================================================================================================
// C-ABI 兼容的公共枚举和数据结构
// ================================================================================================

/// FFI 函数的通用返回码。
#[repr(C)]
#[derive(Debug, PartialEq, Eq)]
pub enum SmtcResult {
    /// 操作成功。
    Success,
    /// 传入的句柄是 NULL 或无效（例如已销毁）。
    InvalidHandle,
    /// 创建 SMTCS 实例失败。
    CreationFailed,
    /// 内部发生错误，通常伴有日志输出。
    InternalError,
}

/// 一个包装器，用于安全地在线程间传递裸指针 `*mut c_void`。
///
/// # 安全性
/// 我们手动（`unsafe`）地为它实现 `Send` 和 `Sync` trait。
/// 这是因为我们向 Rust 编译器作出承诺：C 端的调用者有责任确保
/// 这个指针（`userdata`）在所有可能的回调执行期间（即句柄的整个生命周期内）
/// 都是有效的，并且其指向的数据的访问是线程安全的。
/// 本库仅仅是原样传递此指针，不会对其进行解引用或修改。
#[derive(Copy, Clone)]
struct SendableVoidPtr(*mut c_void);

// 安全性: 见 `SendableVoidPtr` 的文档。C 调用方负责维护安全契约。
unsafe impl Send for SendableVoidPtr {}
unsafe impl Sync for SendableVoidPtr {}

/// C-ABI 兼容的“正在播放”信息结构体。
///
/// # 数据生命周期
/// **此结构体及其指向的所有数据（包括字符串和封面数据）仅在回调函数作用域内有效。**
/// 如果需要在回调函数返回后继续使用这些数据，必须在回调函数内部进行深拷贝。
/// **调用者不应也无需手动释放任何指针。**
#[repr(C)]
#[derive(Debug)]
pub struct CNowPlayingInfo {
    /// 曲目标题 (UTF-8 编码, Null 结尾)。仅在回调内有效。
    pub title: *const c_char,
    /// 艺术家 (UTF-8 编码, Null 结尾)。仅在回调内有效。
    pub artist: *const c_char,
    /// 专辑标题 (UTF-8 编码, Null 结尾)。仅在回调内有效。
    pub album_title: *const c_char,
    /// 歌曲总时长（毫秒）。
    pub duration_ms: u64,
    /// 当前播放位置（毫秒）。
    pub position_ms: u64,
    /// 当前是否正在播放。
    pub is_playing: bool,
    /// 指向封面图片原始数据的指针。仅在回调内有效。
    pub cover_data: *const u8,
    /// 封面图片数据的长度（字节）。
    pub cover_data_len: usize,
    /// 封面图片数据的哈希值，可用于快速比较封面是否变化。
    pub cover_data_hash: u64,
}

/// C-ABI 兼容的 SMTC 会话信息结构体。
///
/// # 数据生命周期
/// **此结构体及其指向的所有字符串数据仅在回调函数作用域内有效。**
/// 如果需要保留，必须在回调函数内部进行深拷贝。
/// **调用者不应也无需手动释放任何指针。**
#[repr(C)]
#[derive(Debug)]
pub struct CSessionInfo {
    /// 会话的唯一 ID (UTF-8 编码, Null 结尾)。仅在回调内有效。
    pub session_id: *const c_char,
    /// 来源应用的 AUMID (UTF-8 编码, Null 结尾)。仅在回调内有效。
    pub source_app_user_model_id: *const c_char,
    /// 用于 UI 显示的名称 (UTF-8 编码, Null 结尾)。仅在回调内有效。
    pub display_name: *const c_char,
}

/// C-ABI 兼容的会话列表结构体，用于在回调中传递会话数组。
///
/// # 数据生命周期
/// **此结构体及其指向的 `sessions` 数组和数组内所有数据仅在回调函数作用域内有效。**
#[repr(C)]
#[derive(Debug)]
pub struct CSessionList {
    /// 指向 `CSessionInfo` 数组头部的指针。
    pub sessions: *const CSessionInfo,
    /// 数组中的元素数量。
    pub count: usize,
}

/// C-ABI 兼容的音频数据包结构体。
///
/// # 数据生命周期
/// **此结构体及其指向的 `data` 数组仅在回调函数作用域内有效。**
#[repr(C)]
#[derive(Debug)]
pub struct CAudioDataPacket {
    /// 指向音频 PCM 数据的指针。
    pub data: *const u8,
    /// 数据长度（字节）。
    pub len: usize,
}

/// C-ABI 兼容的音量变化事件结构体。
///
/// # 数据生命周期
/// **此结构体及其指向的 `session_id` 字符串仅在回调函数作用域内有效。**
/// **调用者不应也无需手动释放任何指针。**
#[repr(C)]
#[derive(Debug)]
pub struct CVolumeChangedEvent {
    /// 发生音量变化的会话 ID (UTF-8 编码, Null 结尾)。仅在回调内有效。
    pub session_id: *const c_char,
    /// 新的音量级别 (0.0 到 1.0)。
    pub volume: f32,
    /// 新的静音状态。
    pub is_muted: bool,
}

/// 定义从 Rust 发送到 C 的更新事件类型。
#[repr(C)]
pub enum CUpdateType {
    /// data 指针类型: `*const CNowPlayingInfo`
    TrackChanged,
    /// data 指针类型: `*const CSessionList`
    SessionsChanged,
    /// data 指针类型: `*const CAudioDataPacket`
    AudioData,
    /// data 指针类型: `*const c_char` (错误信息字符串)
    Error,
    /// data 指针类型: `*const CVolumeChangedEvent`
    VolumeChanged,
    /// data 指针类型: `*const c_char` (已消失会话的 ID)
    SelectedSessionVanished,
}

/// 定义从 C 端接收更新的回调函数指针类型。
///
/// # 参数
/// - `update_type`: 事件的类型，用于决定如何转换 `data` 指针。
/// - `data`: 一个 `const void*` 指针，指向与事件类型对应的 C 结构体。
/// - `userdata`: 调用者在注册时传入的自定义上下文指针。
pub type UpdateCallback =
    extern "C" fn(update_type: CUpdateType, data: *const c_void, userdata: *mut c_void);

// ================================================================================================
// 句柄生命周期管理
// ================================================================================================

/// Rust 端的核心控制器句柄。对于 C 端来说，这是一个不透明指针。
/// 该结构体封装了所有 Rust 资源，包括后台线程和通信通道。
pub struct SmtcHandle {
    controller: Option<MediaController>,
    update_listener_handle: Option<std::thread::JoinHandle<()>>,
    shutdown_signal: Arc<AtomicBool>,
    callback_info: Arc<Mutex<Option<(UpdateCallback, SendableVoidPtr)>>>,
    is_destroyed: AtomicBool,
    listener_creation_mutex: Mutex<()>,
}

/// 宏，用于在 FFI 函数开始时验证句柄的有效性。
macro_rules! validate_handle {
    ($handle_ptr:expr) => {
        if $handle_ptr.is_null() {
            return SmtcResult::InvalidHandle;
        }
        let handle = unsafe { &*$handle_ptr };
        if handle.is_destroyed.load(Ordering::SeqCst) {
            log::warn!("[FFI] 尝试对一个已销毁的句柄进行操作。");
            return SmtcResult::InvalidHandle;
        }
    };
    ($handle_ptr:expr, mut) => {
        if $handle_ptr.is_null() {
            return SmtcResult::InvalidHandle;
        }
        let handle = unsafe { &mut *$handle_ptr };
        if handle.is_destroyed.load(Ordering::SeqCst) {
            log::warn!("[FFI] 尝试对一个已销毁的句柄进行操作。");
            return SmtcResult::InvalidHandle;
        }
    };
}

/// 创建一个新的 SMTC 控制器实例。
///
/// # 参数
/// - `out_handle`: 一个指向 `*mut SmtcHandle` 的指针，用于接收成功创建的句柄。
///
/// # 返回
/// - `SmtcResult::Success` 表示成功，`out_handle` 将被设置为有效的句柄。
/// - `SmtcResult::CreationFailed` 表示失败，`out_handle` 将被设置为 `NULL`。
///
/// # 安全性
/// 调用者有责任确保 `out_handle` 指向一个有效的 `*mut SmtcHandle` 内存位置。
/// 返回的句柄必须在不再需要时通过 `smtc_suite_destroy` 释放，以避免资源泄漏。
/// 导出此函数是安全的，因为它不依赖于任何不安全的前置条件，并且其操作是独立的。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn smtc_suite_create(out_handle: *mut *mut SmtcHandle) -> SmtcResult {
    if out_handle.is_null() {
        return SmtcResult::InternalError;
    }
    // 预先设置为 NULL，这是安全默认值。
    unsafe { *out_handle = std::ptr::null_mut() };

    log::info!("[FFI] 正在调用 smtc_suite_create...");

    match MediaManager::start() {
        Ok(controller) => {
            let handle = Box::new(SmtcHandle {
                controller: Some(controller),
                update_listener_handle: None,
                shutdown_signal: Arc::new(AtomicBool::new(false)),
                callback_info: Arc::new(Mutex::new(None)),
                is_destroyed: AtomicBool::new(false),
                listener_creation_mutex: Mutex::new(()),
            });
            unsafe { *out_handle = Box::into_raw(handle) };
            SmtcResult::Success
        }
        Err(e) => {
            log::error!("[FFI] 创建 SmtcHandle 失败: {e}");
            SmtcResult::CreationFailed
        }
    }
}

/// 销毁 SMTC 控制器实例，并释放所有相关资源。
///
/// 这是一个安全的操作，即使传入 `NULL` 指针也不会导致问题。
/// 此函数会优雅地关闭后台线程。它会同步阻塞，等待回调线程退出，
/// 但最多等待 5 秒。如果回调线程在此时间内未退出（例如被 C 端回调阻塞），
/// 函数将记录警告并返回，这可能导致线程资源泄漏。
///
/// # 安全性
/// - `handle_ptr` 必须是一个由 `smtc_suite_create` 返回且尚未被销毁的有效指针。
/// - 在调用此函数后，`handle_ptr` 将变为无效（悬垂）指针，不应再次使用。
///   导出此函数是安全的，因为它正确处理了 `NULL` 输入并管理其拥有的资源的生命周期。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn smtc_suite_destroy(handle_ptr: *mut SmtcHandle) {
    if handle_ptr.is_null() {
        return;
    }

    let result = catch_unwind(AssertUnwindSafe(|| {
        let handle = unsafe { &mut *handle_ptr };

        if handle
            .is_destroyed
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }

        log::info!("[FFI] 正在销毁 SmtcHandle...");

        handle.shutdown_signal.store(true, Ordering::Release);

        if let Some(controller) = handle.controller.take()
            && let Err(e) = controller.shutdown()
        {
            log::error!("[FFI] 发送关闭命令失败: {e}");
        }

        if let Some(join_handle) = handle.update_listener_handle.take() {
            log::debug!("[FFI] 等待回调监听线程退出...");
            const TIMEOUT: Duration = Duration::from_secs(5);
            let start = Instant::now();

            while !join_handle.is_finished() && start.elapsed() < TIMEOUT {
                std::thread::sleep(Duration::from_millis(50));
            }

            if join_handle.is_finished() {
                if let Err(e) = join_handle.join() {
                    log::error!("[FFI] 等待回调监听线程退出时发生错误: {e:?}");
                } else {
                    log::debug!("[FFI] 回调监听线程已成功退出。");
                }
            } else {
                log::warn!(
                    "[FFI] 监听线程在 {} 秒内未退出，可能因回调阻塞。句柄将被销毁，但线程可能泄漏。",
                    TIMEOUT.as_secs()
                );
            }
        }

        drop(unsafe { Box::from_raw(handle_ptr) });
        log::info!("[FFI] SmtcHandle 已成功销毁。");
    }));

    if result.is_err() {
        log::error!("[FFI] smtc_suite_destroy 内部发生 Panic！");
    }
}

// ================================================================================================
// 回调注册与版本信息
// ================================================================================================

/// 为给定的句柄注册一个回调函数，以接收所有媒体更新。
///
/// 每次调用都会替换掉之前的回调。要注销回调，请传入一个 `NULL` 函数指针。
///
/// # 注意
/// - **线程模型**: 回调函数将在一个由本库管理的**独立后台线程**上被调用。
///   调用者需要确保在回调函数中的所有操作都是线程安全的。
/// - **数据生命周期**: 传递给回调函数的 `data` 指针（例如 `CNowPlayingInfo*`）
///   **仅在回调函数的执行期间有效**。如果需要保留这些数据，必须在回调内部进行深拷贝。
/// - **阻塞警告**: 回调函数不应长时间阻塞，否则可能导致 `smtc_suite_destroy` 调用超时。
///
/// # 参数
/// - `handle_ptr`: 一个由 `smtc_suite_create` 返回的有效句柄。
/// - `callback`: 用于接收更新的函数指针。传入 `NULL` 以注销当前的回调。
/// - `userdata`: 一个用户自定义的上下文指针，它将被原样传递给回调函数。
///
/// # 安全性
/// - `handle_ptr` 必须是一个有效的、尚未被销毁的 `SmtcHandle` 指针。
/// - 调用者必须保证 `userdata` 指针在所有回调的生命周期内都保持有效。
///   导出此函数是安全的，因为它通过句柄与内部状态交互，并对输入进行验证。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn smtc_suite_register_update_callback(
    handle_ptr: *mut SmtcHandle,
    callback: UpdateCallback,
    userdata: *mut c_void,
) -> SmtcResult {
    validate_handle!(handle_ptr, mut);
    let handle = unsafe { &mut *handle_ptr };

    let result = catch_unwind(AssertUnwindSafe(|| {
        // 锁定回调信息，以便安全地在多线程环境中更新它。
        if let Ok(mut cb_info_guard) = handle.callback_info.lock() {
            // 将函数指针转换为 usize 以便检查是否为 NULL。
            let is_callback_present = (callback as usize) != 0;
            if is_callback_present {
                // 如果提供了有效的回调，则存储它和用户数据。
                *cb_info_guard = Some((callback, SendableVoidPtr(userdata)));
            } else {
                // 如果传入 NULL，则清空回调信息，禁用回调。
                *cb_info_guard = None;
            }
        } else {
            // 如果互斥锁被“毒化”（即持有锁的线程 panic 了），这是一个严重的内部错误。
            log::error!("[FFI] 回调信息锁已被毒化，无法更新回调。");
            return SmtcResult::InternalError;
        }

        let is_callback_present = (callback as usize) != 0;
        if is_callback_present {
            // 使用互斥锁确保创建监听线程的操作是原子的。
            // 这可以防止在多线程环境中（尽管不常见）同时多次调用此函数导致创建多个监听线程。
            let _creation_guard = match handle.listener_creation_mutex.lock() {
                Ok(guard) => guard,
                Err(_) => {
                    log::error!("[FFI] 监听器创建锁已被毒化，操作中止。");
                    return SmtcResult::InternalError;
                }
            };

            // 只有在第一次注册有效回调且监听线程尚未运行时，才创建它。
            if handle.update_listener_handle.is_none() {
                log::info!("[FFI] 首次注册回调，正在启动回调监听线程...");

                // 从 MediaController 中“拿走”更新接收器 (`update_rx`)。
                // 我们使用 `std::mem::replace` 将其替换为一个新的、空的通道接收器，
                // 以保持 MediaController 结构体的有效性，同时将接收器的所有权转移到新线程。
                let update_rx: CrossbeamReceiver<MediaUpdate> = match handle.controller.as_mut() {
                    Some(c) => {
                        std::mem::replace(&mut c.update_rx, crossbeam_channel::unbounded().1)
                    }
                    None => {
                        log::error!("[FFI] MediaController 无效或已被销毁，无法启动监听线程。");
                        return SmtcResult::InternalError;
                    }
                };

                // 克隆需要在新线程中使用的共享资源。
                let shutdown_signal = handle.shutdown_signal.clone();
                let callback_info = handle.callback_info.clone();

                // 创建并启动回调监听线程。
                let join_handle = std::thread::spawn(move || {
                    log::debug!(
                        "[FFI 回调线程] 线程已启动 (ID: {:?})。",
                        std::thread::current().id()
                    );

                    // --- 核心事件循环 ---
                    loop {
                        // 优先检查关闭信号，确保能够快速响应关闭请求。
                        if shutdown_signal.load(Ordering::Acquire) {
                            log::debug!("[FFI 回调线程] 收到关闭信号，准备退出。");
                            break;
                        }

                        // 使用 `crossbeam_channel::select!` 宏来同时等待消息或超时。
                        select! {
                            // 分支 1: 尝试从 `update_rx` 接收消息。
                            recv(update_rx) -> msg => {
                                match msg {
                                    Ok(update) => {
                                        // 成功接收到一条更新消息。
                                        // 再次锁定回调信息，以获取最新的回调函数指针和用户数据。
                                        if let Ok(cb_info_guard) = callback_info.lock()
                                            && let Some((cb_fn, user_ptr_wrapper)) = *cb_info_guard
                                        {
                                            // 使用 `catch_unwind` 包装对 C 回调的调用，
                                            // 以防止 C 端的 panic 破坏我们的 Rust 线程。
                                            let res = catch_unwind(|| unsafe {
                                                process_and_invoke_callback(
                                                    update,
                                                    cb_fn,
                                                    user_ptr_wrapper.0,
                                                );
                                            });
                                            if res.is_err() {
                                                log::error!(
                                                    "[FFI 回调线程] C 端回调函数发生 Panic！线程将继续运行。"
                                                );
                                            }
                                        }
                                    },
                                    Err(_) => {
                                        // `recv` 返回 `Err` 意味着通道的发送端已经全部被丢弃（disconnected）。
                                        // 这通常发生在 `MediaWorker` 线程退出时，是正常的关闭流程。
                                        log::warn!("[FFI 回调线程] 更新通道已断开，线程退出。");
                                        break; // 退出循环，结束线程。
                                    }
                                }
                            },
                            // 分支 2: 等待超时。
                            // `default(duration)` 是一个特殊的 select 分支，
                            // 如果在指定的 `duration` 内没有任何其他分支准备好，它就会被执行。
                            default(Duration::from_millis(100)) => {
                                // 超时发生。我们什么都不做，直接进入下一次循环迭代。
                                continue;
                            }
                        }
                    }
                    log::debug!(
                        "[FFI 回调线程] 线程已结束 (ID: {:?})。",
                        std::thread::current().id()
                    );
                });

                // 将新创建的线程句柄存储在 SmtcHandle 中，以便将来可以 `join` 它。
                handle.update_listener_handle = Some(join_handle);
            }
        } else {
            // 如果传入的回调是 NULL，仅打印一条日志信息。
            // 线程的停止逻辑由 `smtc_suite_destroy` 统一处理，不需要主动停止线程。
            log::info!(
                "[FFI] 传入 NULL 回调，回调功能已禁用。监听线程（如果存在）将继续运行但不会调用任何回调。"
            );
        }

        SmtcResult::Success
    }));

    // 如果 `catch_unwind` 捕获到了 panic，返回一个内部错误码。
    result.unwrap_or_else(|_| {
        log::error!("[FFI] `smtc_suite_register_update_callback` 内部发生 Panic！");
        SmtcResult::InternalError
    })
}

/// 获取当前库的版本字符串。
///
/// # 返回
/// 一个指向静态 UTF-8 字符串的指针，表示库的版本（例如 "0.1.0"）。
/// 该指针永久有效，调用者无需释放。
///
/// # 安全性
/// 导出此函数是安全的，因为它不接受任何输入并返回一个静态的、常量的数据。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn smtc_suite_get_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr() as *const c_char
}

// ================================================================================================
// 命令函数
// ================================================================================================

/// 向 SMTC 套件发送一个媒体控制命令。
///
/// # 安全性
/// `handle_ptr` 必须是一个由 `smtc_suite_create` 返回的有效指针。
/// 导出此函数是安全的，因为它通过句柄与内部状态交互，并对输入进行验证。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn smtc_suite_control_command(
    handle_ptr: *mut SmtcHandle,
    command: CSmtcControlCommand,
) -> SmtcResult {
    validate_handle!(handle_ptr);
    let handle = unsafe { &*handle_ptr };

    let res = catch_unwind(AssertUnwindSafe(|| {
        if let Some(controller) = &handle.controller {
            let rust_command = match command.command_type {
                CControlCommandType::Pause => SmtcControlCommand::Pause,
                CControlCommandType::Play => SmtcControlCommand::Play,
                CControlCommandType::SkipNext => SmtcControlCommand::SkipNext,
                CControlCommandType::SkipPrevious => SmtcControlCommand::SkipPrevious,
                CControlCommandType::SeekTo => {
                    SmtcControlCommand::SeekTo(unsafe { command.data.seek_to_ms })
                }
                CControlCommandType::SetVolume => {
                    SmtcControlCommand::SetVolume(unsafe { command.data.volume_level })
                }
                CControlCommandType::SetShuffle => {
                    SmtcControlCommand::SetShuffle(unsafe { command.data.is_shuffle_active })
                }
                CControlCommandType::SetRepeatMode => {
                    let c_mode = unsafe { command.data.repeat_mode };
                    let rust_mode = match c_mode {
                        CRepeatMode::Off => RepeatMode::Off,
                        CRepeatMode::One => RepeatMode::One,
                        CRepeatMode::All => RepeatMode::All,
                    };
                    SmtcControlCommand::SetRepeatMode(rust_mode)
                }
            };
            if controller
                .command_tx
                .send(MediaCommand::Control(rust_command))
                .is_err()
            {
                log::error!("[FFI] 发送控制命令失败: 通道已关闭。");
                return SmtcResult::InternalError;
            }
        }
        SmtcResult::Success
    }));

    res.unwrap_or_else(|_| {
        log::error!("[FFI] smtc_suite_control_command 内部发生 Panic！");
        SmtcResult::InternalError
    })
}

/// 启用或禁用高频进度更新。
///
/// 当启用时，库会以 100ms 的固定间隔主动发送 `TrackChanged` 更新事件，
/// 以便实现平滑的进度条。禁用后，`TrackChanged` 事件仅在 SMTC
/// 报告真实变化时才发送。
///
/// # 参数
/// - `handle_ptr`: 一个由 `smtc_suite_create` 返回的有效句柄。
/// - `enabled`: `true` 表示启用高频更新，`false` 表示禁用。
///
/// # 返回
/// - `SmtcResult::Success` 表示命令已成功发送。
/// - `SmtcResult::InvalidHandle` 如果句柄无效。
/// - `SmtcResult::InternalError` 如果命令发送失败（例如后台线程已关闭）。
///
/// # 安全性
/// `handle_ptr` 必须是一个由 `smtc_suite_create` 返回的有效指针。
/// 导出此函数是安全的，因为它通过句柄与内部状态交互，并对输入进行验证。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn smtc_suite_set_high_frequency_progress_updates(
    handle_ptr: *mut SmtcHandle,
    enabled: bool,
) -> SmtcResult {
    validate_handle!(handle_ptr);
    let handle = unsafe { &*handle_ptr };

    let res = catch_unwind(AssertUnwindSafe(|| {
        if let Some(controller) = &handle.controller
            && controller
                .command_tx
                .send(MediaCommand::SetHighFrequencyProgressUpdates(enabled))
                .is_err()
            {
                log::error!("[FFI] 发送设置高频更新命令失败: 通道已关闭。");
                return SmtcResult::InternalError;
            }
        SmtcResult::Success
    }));

    res.unwrap_or_else(|_| {
        log::error!("[FFI] smtc_suite_set_high_frequency_progress_updates 内部发生 Panic！");
        SmtcResult::InternalError
    })
}

/// 选择一个 SMTC 会话进行监控。
///
/// # 参数
/// - `session_id`: 目标会话的 ID (UTF-8 编码, Null 结尾)。传入空字符串或 `NULL` 以切换到自动选择模式。
///
/// # 安全性
/// `handle_ptr` 必须有效。如果 `session_id` 非 `NULL`，它必须指向一个有效的 C 字符串。
/// 导出此函数是安全的，因为它通过句柄与内部状态交互，并对输入进行验证。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn smtc_suite_select_session(
    handle_ptr: *mut SmtcHandle,
    session_id: *const c_char,
) -> SmtcResult {
    validate_handle!(handle_ptr);
    let handle = unsafe { &*handle_ptr };

    let res = catch_unwind(AssertUnwindSafe(|| {
        let session_id_str = if session_id.is_null() {
            String::new()
        } else {
            unsafe { CStr::from_ptr(session_id).to_string_lossy().into_owned() }
        };

        if let Some(controller) = &handle.controller
            && controller
                .command_tx
                .send(MediaCommand::SelectSession(session_id_str))
                .is_err()
        {
            log::error!("[FFI] 发送选择会话命令失败: 通道已关闭。");
            return SmtcResult::InternalError;
        }
        SmtcResult::Success
    }));

    res.unwrap_or_else(|_| {
        log::error!("[FFI] smtc_suite_select_session 内部发生 Panic！");
        SmtcResult::InternalError
    })
}

/// 设置 SMTC 元数据的文本转换模式。
///
/// 这是从 C 语言调用以控制简繁转换行为的函数。
///
/// # 安全性
///
/// 调用者必须确保 `controller_ptr` 是一个由 `smtc_start` 返回的有效指针，
/// 并且在调用此函数时没有被释放。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn smtc_set_text_conversion_mode(
    controller_ptr: *mut MediaController,
    mode: CTextConversionMode,
) -> bool {
    let Some(controller) = (unsafe { controller_ptr.as_ref() }) else {
        return false;
    };

    // 将 C 枚举转换为 Rust 枚举
    let rust_mode = TextConversionMode::from(mode);

    // 创建并发送命令
    let command = MediaCommand::SetTextConversion(rust_mode);
    controller.command_tx.send(command).is_ok()
}

/// 请求开始音频捕获。
///
/// # 安全性
/// `handle_ptr` 必须是一个由 `smtc_suite_create` 返回的有效指针。
/// 导出此函数是安全的，因为它通过句柄与内部状态交互，并对输入进行验证。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn smtc_suite_start_audio_capture(handle_ptr: *mut SmtcHandle) -> SmtcResult {
    validate_handle!(handle_ptr);
    let handle = unsafe { &*handle_ptr };

    if let Some(controller) = &handle.controller
        && controller
            .command_tx
            .send(MediaCommand::StartAudioCapture)
            .is_err()
    {
        log::error!("[FFI] 发送开始音频捕获命令失败: 通道已关闭。");
        return SmtcResult::InternalError;
    }
    SmtcResult::Success
}

/// 请求停止音频捕获。
///
/// # 安全性
/// `handle_ptr` 必须是一个由 `smtc_suite_create` 返回的有效指针。
/// 导出此函数是安全的，因为它通过句柄与内部状态交互，并对输入进行验证。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn smtc_suite_stop_audio_capture(handle_ptr: *mut SmtcHandle) -> SmtcResult {
    validate_handle!(handle_ptr);
    let handle = unsafe { &*handle_ptr };

    if let Some(controller) = &handle.controller
        && controller
            .command_tx
            .send(MediaCommand::StopAudioCapture)
            .is_err()
    {
        log::error!("[FFI] 发送停止音频捕获命令失败: 通道已关闭。");
        return SmtcResult::InternalError;
    }
    SmtcResult::Success
}

// ================================================================================================
// 内存管理 (仅限内部使用)
// ================================================================================================

/// 释放由本库 FFI 函数返回的字符串 (`*mut c_char`)。
///
/// # 安全性
/// `s` 必须是一个由本库的 FFI 函数返回、且尚未被释放的有效指针。
/// 传入 `NULL` 是安全的。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn smtc_suite_free_string(s: *mut c_char) {
    if s.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| {
        drop(unsafe { CString::from_raw(s) });
    }));
}

// ================================================================================================
// 内部辅助函数与 RAII 守卫
// ================================================================================================

// RAII 守卫，确保 CString 在作用域结束时（包括 panic）被正确释放。
struct StringGuard(*mut c_char);
impl Drop for StringGuard {
    fn drop(&mut self) {
        unsafe { smtc_suite_free_string(self.0) };
    }
}

// RAII 守卫，确保 CNowPlayingInfo 的所有字符串成员都被释放。
struct NowPlayingInfoGuard(CNowPlayingInfo);
impl Drop for NowPlayingInfoGuard {
    fn drop(&mut self) {
        unsafe {
            smtc_suite_free_string(self.0.title as *mut c_char);
            smtc_suite_free_string(self.0.artist as *mut c_char);
            smtc_suite_free_string(self.0.album_title as *mut c_char);
        }
    }
}

// RAII 守卫，确保 CSessionList 中所有 CSessionInfo 的字符串成员都被释放。
struct SessionListGuard(Vec<CSessionInfo>);
impl Drop for SessionListGuard {
    fn drop(&mut self) {
        for session in self.0.drain(..) {
            unsafe {
                smtc_suite_free_string(session.session_id as *mut c_char);
                smtc_suite_free_string(session.source_app_user_model_id as *mut c_char);
                smtc_suite_free_string(session.display_name as *mut c_char);
            }
        }
    }
}

// RAII 守卫，确保 CVolumeChangedEvent 的字符串成员被释放。
struct VolumeChangedEventGuard(CVolumeChangedEvent);
impl Drop for VolumeChangedEventGuard {
    fn drop(&mut self) {
        unsafe {
            smtc_suite_free_string(self.0.session_id as *mut c_char);
        }
    }
}

/// (内部) 将 Rust 的 `MediaUpdate` 转换为 C 兼容结构，并调用回调函数。
///
/// # 安全性
/// `callback` 必须是一个有效的函数指针, `userdata` 必须是有效的指针。
/// 此函数通过 RAII 守卫管理传递给回调的数据的生命周期，确保资源安全。
unsafe fn process_and_invoke_callback(
    update: MediaUpdate,
    callback: UpdateCallback,
    userdata: *mut c_void,
) {
    match update {
        MediaUpdate::TrackChanged(info) => {
            let c_info = convert_to_c_now_playing_info(&info);
            let _guard = NowPlayingInfoGuard(c_info);
            callback(
                CUpdateType::TrackChanged,
                &_guard.0 as *const _ as *const c_void,
                userdata,
            );
        }
        MediaUpdate::SessionsChanged(sessions) => {
            let c_sessions: Vec<CSessionInfo> =
                sessions.iter().map(convert_to_c_session_info).collect();
            let _guard = SessionListGuard(c_sessions);
            let list = CSessionList {
                sessions: _guard.0.as_ptr(),
                count: _guard.0.len(),
            };
            callback(
                CUpdateType::SessionsChanged,
                &list as *const _ as *const c_void,
                userdata,
            );
        }
        MediaUpdate::AudioData(bytes) => {
            let packet = CAudioDataPacket {
                data: bytes.as_ptr(),
                len: bytes.len(),
            };
            callback(
                CUpdateType::AudioData,
                &packet as *const _ as *const c_void,
                userdata,
            );
        }
        MediaUpdate::Error(err_msg) => {
            let c_err_msg = to_c_char(&err_msg);
            let _guard = StringGuard(c_err_msg);
            callback(CUpdateType::Error, c_err_msg as *const c_void, userdata);
        }
        MediaUpdate::VolumeChanged {
            session_id,
            volume,
            is_muted,
        } => {
            let event = CVolumeChangedEvent {
                session_id: to_c_char(&session_id),
                volume,
                is_muted,
            };
            let _guard = VolumeChangedEventGuard(event);
            callback(
                CUpdateType::VolumeChanged,
                &_guard.0 as *const _ as *const c_void,
                userdata,
            );
        }
        MediaUpdate::SelectedSessionVanished(session_id) => {
            let c_session_id = to_c_char(&session_id);
            let _guard = StringGuard(c_session_id);
            callback(
                CUpdateType::SelectedSessionVanished,
                c_session_id as *const c_void,
                userdata,
            );
        }
    }
}

/// (内部) 将 Rust 的字符串切片安全地转换为 C 的 `*mut c_char`。
/// 如果输入字符串包含内部 NUL 字节，它会被截断。
fn to_c_char<S: AsRef<str>>(s: S) -> *mut c_char {
    CString::new(s.as_ref()).unwrap_or_default().into_raw()
}

/// (内部) 将 Rust 的 `NowPlayingInfo` 转换为 C 的 `CNowPlayingInfo`。
fn convert_to_c_now_playing_info(info: &NowPlayingInfo) -> CNowPlayingInfo {
    CNowPlayingInfo {
        title: to_c_char(info.title.as_deref().unwrap_or("")),
        artist: to_c_char(info.artist.as_deref().unwrap_or("")),
        album_title: to_c_char(info.album_title.as_deref().unwrap_or("")),
        duration_ms: info.duration_ms.unwrap_or(0),
        position_ms: info.position_ms.unwrap_or(0),
        is_playing: info.is_playing.unwrap_or(false),
        cover_data: info
            .cover_data
            .as_ref()
            .map_or(std::ptr::null(), |d| d.as_ptr()),
        cover_data_len: info.cover_data.as_ref().map_or(0, |d| d.len()),
        cover_data_hash: info.cover_data_hash.unwrap_or(0),
    }
}

/// (内部) 将 Rust 的 `SmtcSessionInfo` 转换为 C 的 `CSessionInfo`。
fn convert_to_c_session_info(info: &SmtcSessionInfo) -> CSessionInfo {
    CSessionInfo {
        session_id: to_c_char(&info.session_id),
        source_app_user_model_id: to_c_char(&info.source_app_user_model_id),
        display_name: to_c_char(&info.display_name),
    }
}
