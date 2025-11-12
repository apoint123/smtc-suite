use crate::{
    MediaController,
    api::{
        CControlCommandType, CSmtcControlCommand, CTextConversionMode, MediaCommand, MediaUpdate,
        NowPlayingInfo, RepeatMode, SmtcControlCommand, SmtcSessionInfo,
    },
};
use log::{Level, LevelFilter, Metadata, Record};
use std::{
    ffi::{CStr, CString, c_char, c_void},
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::runtime::Runtime as TokioRuntime;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[repr(C)]
#[derive(Debug, PartialEq, Eq)]
pub enum SmtcResult {
    Success,
    InvalidHandle,
    CreationFailed,
    InternalError,
    ChannelFull,
}

/// A wrapper for safely passing a raw pointer `*mut c_void` between threads.
///
/// # Safety
/// The C caller is responsible to ensure that this pointer (`userdata`) is
/// valid during all possible callback executions (i.e., the lifetime of the
/// handle) and that access to the data it points to is thread-safe. We simply
/// passes this pointer around without dereferencing or modifying it.
#[derive(Copy, Clone)]
struct SendableVoidPtr(*mut c_void);

unsafe impl Send for SendableVoidPtr {}
unsafe impl Sync for SendableVoidPtr {}

/// C-ABI compatible "now playing" information struct.
///
/// # Data Lifetime
/// This struct and all the data it points to (including strings) are only
/// valid within the scope of the callback function.
/// If you need to use this data after the callback function returns, you must
/// perform a deep copy inside the callback.
/// The caller should not and does not need to manually free any pointers.
#[repr(C)]
#[derive(Debug)]
pub struct CNowPlayingInfo {
    /// Track title (UTF-8 encoded, Null-terminated). Only valid within the
    /// callback.
    pub title: *const c_char,
    /// Artist (UTF-8 encoded, Null-terminated). Only valid within the callback.
    pub artist: *const c_char,
    /// Album title (UTF-8 encoded, Null-terminated). Only valid within the
    /// callback.
    pub album_title: *const c_char,
    /// Total duration of the song in milliseconds.
    pub duration_ms: u64,
    /// Current playback position in milliseconds.
    pub position_ms: u64,
    /// Whether it is currently playing.
    pub is_playing: bool,
}

/// C-ABI compatible SMTC session information struct.
///
/// # Data Lifetime
/// This struct and all the string data it points to are only valid within the
/// scope of the callback function.
/// If you need to retain it, you must perform a deep copy inside the callback.
/// The caller should not and does not need to manually free any pointers.
#[repr(C)]
#[derive(Debug)]
pub struct CSessionInfo {
    /// The unique ID of the session (UTF-8 encoded, Null-terminated). Only
    /// valid within the callback.
    pub session_id: *const c_char,
    /// The AUMID of the source application (UTF-8 encoded, Null-terminated).
    /// Only valid within the callback.
    pub source_app_user_model_id: *const c_char,
    /// The display name for UI purposes (UTF-8 encoded, Null-terminated). Only
    /// valid within the callback.
    pub display_name: *const c_char,
}

/// C-ABI compatible session list struct, used for passing an array of sessions
/// in a callback.
///
/// # Data Lifetime
/// This struct, the `sessions` array it points to, and all data within that
/// array are only valid within the scope of the callback function.
#[repr(C)]
#[derive(Debug)]
pub struct CSessionList {
    /// Pointer to the head of the `CSessionInfo` array.
    pub sessions: *const CSessionInfo,
    /// The number of elements in the array.
    pub count: usize,
}

/// C-ABI compatible diagnostic level enum.
#[repr(C)]
#[derive(Debug)]
pub enum CDiagnosticLevel {
    Warning,
    Error,
}

/// C-ABI compatible diagnostic information struct.
///
/// # Data Lifetime
/// This struct and all the string data it points to are only valid within the
/// scope of the callback function.
/// If you need to retain it, you must perform a deep copy inside the callback.
/// The caller should not and does not need to manually free any pointers.
#[repr(C)]
#[derive(Debug)]
pub struct CDiagnosticInfo {
    /// The level of the diagnostic event.
    pub level: CDiagnosticLevel,
    /// The specific content of the diagnostic message. Only valid within the
    /// callback.
    pub message: *const c_char,
    /// The timestamp of when the event occurred. Only valid within the
    /// callback.
    pub timestamp_str: *const c_char,
}

/// C-ABI compatible audio data packet struct.
///
/// # Data Lifetime
/// This struct and the `data` array it points to are only valid within the
/// scope of the callback function.
#[repr(C)]
#[derive(Debug)]
pub struct CAudioDataPacket {
    /// Pointer to the audio PCM data.
    pub data: *const u8,
    /// The length of the data in bytes.
    pub len: usize,
}

/// C-ABI compatible volume changed event struct.
///
/// # Data Lifetime
/// This struct and the `session_id` string it points to are only valid within
/// the scope of the callback function.
/// The caller should not and does not need to manually free any pointers.
#[repr(C)]
#[derive(Debug)]
pub struct CVolumeChangedEvent {
    /// The ID of the session where the volume changed (UTF-8 encoded,
    /// Null-terminated). Only valid within the callback.
    pub session_id: *const c_char,
    /// The new volume level (from 0.0 to 1.0).
    pub volume: f32,
    /// The new mute state.
    pub is_muted: bool,
}

/// Defines the update event types sent from Rust to C.
#[repr(C)]
pub enum CUpdateType {
    /// data pointer type: `*const CNowPlayingInfo` (regular update)
    TrackChanged,
    /// data pointer type: `*const CSessionList`
    SessionsChanged,
    /// data pointer type: `*const CAudioDataPacket`
    AudioData,
    /// data pointer type: `*const c_char` (error message string)
    Error,
    /// data pointer type: `*const CVolumeChangedEvent`
    VolumeChanged,
    /// data pointer type: `*const c_char` (ID of the vanished session)
    SelectedSessionVanished,
    /// data pointer type: `*const CDiagnosticInfo`
    Diagnostic,
}

/// Defines the callback function pointer type for receiving updates from the C
/// side.
///
/// # Arguments
/// - `update_type`: The type of the event, used to determine how to cast the
///   `data` pointer.
/// - `data`: A `const void*` pointer to a C struct corresponding to the event
///   type.
/// - `userdata`: The custom context pointer passed by the caller during
///   registration.
pub type UpdateCallback =
    extern "C" fn(update_type: CUpdateType, data: *const c_void, userdata: *mut c_void);

/// C-ABI compatible log level enum.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub enum CLogLevel {
    Error = 1,
    Warn = 2,
    Info = 3,
    Debug = 4,
    Trace = 5,
}

impl From<CLogLevel> for LevelFilter {
    fn from(level: CLogLevel) -> Self {
        match level {
            CLogLevel::Error => Self::Error,
            CLogLevel::Warn => Self::Warn,
            CLogLevel::Info => Self::Info,
            CLogLevel::Debug => Self::Debug,
            CLogLevel::Trace => Self::Trace,
        }
    }
}

impl From<Level> for CLogLevel {
    fn from(level: Level) -> Self {
        match level {
            Level::Error => Self::Error,
            Level::Warn => Self::Warn,
            Level::Info => Self::Info,
            Level::Debug => Self::Debug,
            Level::Trace => Self::Trace,
        }
    }
}

/// Defines the pointer type for the C side log callback function.
///
/// # Arguments
/// - `level`: The level of the log message.
/// - `target`: The module path of the log source (e.g., "`my_lib::my_module`").
/// - `message`: The UTF-8 encoded, Null-terminated log message.
/// - `userdata`: The custom context pointer passed by the caller during
///   registration.
pub type LogCallback = extern "C" fn(
    level: CLogLevel,
    target: *const c_char,
    message: *const c_char,
    userdata: *mut c_void,
);

/// State for the FFI logger, used to store the C callback and user data.
struct FfiLoggerState {
    callback: LogCallback,
    userdata: SendableVoidPtr,
}

static FFI_LOGGER: OnceLock<FfiLogger> = OnceLock::new();

struct FfiLogger {
    state: Mutex<FfiLoggerState>,
}

impl log::Log for FfiLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }

        let Ok(state_guard) = self.state.lock() else {
            return;
        };

        let target = CString::new(record.target()).unwrap_or_default();
        let message = CString::new(record.args().to_string()).unwrap_or_default();

        (state_guard.callback)(
            record.level().into(),
            target.as_ptr(),
            message.as_ptr(),
            state_guard.userdata.0,
        );
    }

    fn flush(&self) {}
}

// ================================================================================================
// Handle Lifetime Management
// ================================================================================================

/// The core controller handle on the Rust side.
pub struct SmtcHandle {
    controller: Option<MediaController>,
    update_rx: Option<mpsc::Receiver<MediaUpdate>>,
    runtime: Option<TokioRuntime>,
    update_listener_handle: Option<std::thread::JoinHandle<()>>,
    shutdown_token: CancellationToken,
    callback_info: Arc<Mutex<Option<(UpdateCallback, SendableVoidPtr)>>>,
    is_destroyed: AtomicBool,
    listener_creation_mutex: Mutex<()>,
}

/// Macro to validate the handle at the beginning of an FFI function.
macro_rules! validate_handle {
    ($handle_ptr:expr) => {
        if $handle_ptr.is_null() {
            return SmtcResult::InvalidHandle;
        }
        let handle = unsafe { &*$handle_ptr };
        if handle.is_destroyed.load(Ordering::SeqCst) {
            log::warn!("[FFI] Attempted to operate on a destroyed handle.");
            return SmtcResult::InvalidHandle;
        }
    };
    ($handle_ptr:expr, mut) => {
        if $handle_ptr.is_null() {
            return SmtcResult::InvalidHandle;
        }
        let handle = unsafe { &mut *$handle_ptr };
        if handle.is_destroyed.load(Ordering::SeqCst) {
            log::warn!("[FFI] Attempted to operate on a destroyed handle.");
            return SmtcResult::InvalidHandle;
        }
    };
}

/// Creates a new SMTC controller instance.
///
/// # Arguments
/// - `out_handle`: A pointer to `*mut SmtcHandle` to receive the successfully
///   created handle.
///
/// # Returns
/// - `SmtcResult::Success` on success, `out_handle` will be set to a valid
///   handle.
/// - `SmtcResult::CreationFailed` on failure, `out_handle` will be set to
///   `NULL`.
///
/// # Safety
/// The caller is responsible for ensuring `out_handle` points to a valid memory
/// location for a `*mut SmtcHandle`. The returned handle must be freed via
/// `smtc_suite_destroy` when no longer needed to avoid resource leaks.
/// Exporting this function is safe as it has no unsafe preconditions and its
/// operation is self-contained.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn smtc_suite_create(out_handle: *mut *mut SmtcHandle) -> SmtcResult {
    if out_handle.is_null() {
        return SmtcResult::InternalError;
    }
    // Pre-emptively set to NULL as a safe default.
    unsafe { *out_handle = std::ptr::null_mut() };

    log::info!("[FFI] Calling smtc_suite_create...");

    match crate::MediaManager::start() {
        Ok((controller, update_rx)) => {
            let runtime = match TokioRuntime::new() {
                Ok(rt) => rt,
                Err(e) => {
                    log::error!("[FFI] Failed to create Tokio runtime: {e}");
                    return SmtcResult::CreationFailed;
                }
            };

            let handle = Box::new(SmtcHandle {
                controller: Some(controller),
                update_rx: Some(update_rx),
                runtime: Some(runtime),
                update_listener_handle: None,
                shutdown_token: CancellationToken::new(),
                callback_info: Arc::new(Mutex::new(None)),
                is_destroyed: AtomicBool::new(false),
                listener_creation_mutex: Mutex::new(()),
            });
            unsafe { *out_handle = Box::into_raw(handle) };
            SmtcResult::Success
        }
        Err(e) => {
            log::error!("[FFI] Failed to create SmtcHandle: {e}");
            SmtcResult::CreationFailed
        }
    }
}

/// Destroys the SMTC controller instance and releases all associated resources.
///
/// This is a safe operation, even if a `NULL` pointer is passed.
/// This function gracefully shuts down the background thread. It will block
/// synchronously to wait for the callback thread to exit, but for a maximum of
/// 5 seconds. If the callback thread does not exit within this time (e.g.,
/// blocked by a C-side callback), the function will log a warning and return,
/// which may lead to a thread resource leak.
///
/// # Safety
/// - `handle_ptr` must be a valid pointer returned by `smtc_suite_create` that
///   has not yet been destroyed.
/// - After calling this function, `handle_ptr` becomes an invalid (dangling)
///   pointer and should not be used again. Exporting this function is safe as
///   it correctly handles `NULL` input and manages the lifecycle of the
///   resources it owns.
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

        log::info!("[FFI] Destroying SmtcHandle...");
        handle.shutdown_token.cancel();

        if let (Some(controller), Some(runtime)) = (handle.controller.take(), handle.runtime.take())
        {
            runtime.block_on(async {
                if let Err(e) = controller.shutdown().await {
                    log::error!("[FFI] Failed to send shutdown command: {e}");
                }
            });
        }

        if let Some(join_handle) = handle.update_listener_handle.take() {
            log::debug!("[FFI] Waiting for callback listener thread to exit...");
            const TIMEOUT: Duration = Duration::from_secs(5);
            let start = Instant::now();

            while !join_handle.is_finished() && start.elapsed() < TIMEOUT {
                std::thread::sleep(Duration::from_millis(50));
            }

            if join_handle.is_finished() {
                if let Err(e) = join_handle.join() {
                    log::error!(
                        "[FFI] Error occurred while waiting for callback listener thread to exit: {e:?}"
                    );
                } else {
                    log::debug!("[FFI] Callback listener thread has exited successfully.");
                }
            } else {
                log::warn!(
                    "[FFI] Listener thread did not exit within {} seconds, possibly due to a blocking callback. The handle will be destroyed, but the thread may leak.",
                    TIMEOUT.as_secs()
                );
            }
        }

        drop(unsafe { Box::from_raw(handle_ptr) });
        log::info!("[FFI] SmtcHandle has been successfully destroyed.");
    }));

    if result.is_err() {
        log::error!("[FFI] Panic occurred inside smtc_suite_destroy!");
    }
}

// ================================================================================================
// Callback Registration & Version Info
// ================================================================================================

/// Registers a callback function for the given handle to receive all media
/// updates.
///
/// Each call will replace the previous callback. To unregister, pass a `NULL`
/// function pointer.
///
/// # Notes
/// - Threading Model: The callback function will be invoked on a separate
///   background thread managed by this library. The caller must ensure that all
///   operations within the callback are thread-safe.
/// - Data Lifetime: The `data` pointer passed to the callback (e.g.,
///   `CNowPlayingInfo*`) is only valid for the duration of the callback's
///   execution. If you need to retain this data, you must perform a deep copy
///   inside the callback.
/// - Blocking Warning: The callback function should not block for long periods,
///   as this could cause the `smtc_suite_destroy` call to time out.
///
/// # Arguments
/// - `handle_ptr`: A valid handle returned by `smtc_suite_create`.
/// - `callback`: A function pointer to receive updates. Pass `NULL` to
///   unregister the current callback.
/// - `userdata`: A user-defined context pointer that will be passed back to the
///   callback as-is.
///
/// # Safety
/// - `handle_ptr` must be a valid, non-destroyed `SmtcHandle` pointer.
/// - The caller must guarantee that the `userdata` pointer remains valid for
///   the entire lifetime of any callback. Exporting this function is safe as it
///   interacts with internal state via the handle and validates its inputs.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn smtc_suite_register_update_callback(
    handle_ptr: *mut SmtcHandle,
    callback: UpdateCallback,
    userdata: *mut c_void,
) -> SmtcResult {
    validate_handle!(handle_ptr, mut);
    let handle = unsafe { &mut *handle_ptr };

    let result = catch_unwind(AssertUnwindSafe(|| {
        if let Ok(mut cb_info_guard) = handle.callback_info.lock() {
            let is_callback_present = (callback as usize) != 0;
            if is_callback_present {
                *cb_info_guard = Some((callback, SendableVoidPtr(userdata)));
            } else {
                *cb_info_guard = None;
            }
        } else {
            log::error!("[FFI] Callback info lock is poisoned, cannot update callback.");
            return SmtcResult::InternalError;
        }

        let is_callback_present = (callback as usize) != 0;
        if is_callback_present {
            let Ok(_creation_guard) = handle.listener_creation_mutex.lock() else {
                log::error!("[FFI] Listener creation lock is poisoned, aborting operation.");
                return SmtcResult::InternalError;
            };

            if handle.update_listener_handle.is_none() {
                log::info!(
                    "[FFI] First time registering a callback, starting the callback listener thread..."
                );

                let Some(mut update_rx) = handle.update_rx.take() else {
                    log::error!(
                        "[FFI] update_rx has already been moved, cannot create listener thread again."
                    );
                    return SmtcResult::InternalError;
                };

                let shutdown_token = handle.shutdown_token.clone();
                let callback_info = handle.callback_info.clone();

                let join_handle = std::thread::spawn(move || {
                    log::debug!(
                        "[FFI Callback Thread] Thread started (ID: {:?}).",
                        std::thread::current().id()
                    );

                    let rt = match TokioRuntime::new() {
                        Ok(rt) => rt,
                        Err(e) => {
                            log::error!(
                                "[FFI Callback Thread] Failed to create Tokio runtime: {e}"
                            );
                            return;
                        }
                    };

                    rt.block_on(async move {
                        loop {
                            tokio::select! {
                                biased;

                                maybe_update = update_rx.recv() => {
                                    if let Some(update) = maybe_update {
                                        if let Ok(cb_info_guard) = callback_info.lock()
                                            && let Some((cb_fn, user_ptr_wrapper)) = *cb_info_guard
                                        {
                                            let res = catch_unwind(|| unsafe {
                                                process_and_invoke_callback(
                                                    update,
                                                    cb_fn,
                                                    user_ptr_wrapper.0,
                                                );
                                            });
                                            if res.is_err() {
                                                log::error!(
                                                    "[FFI Callback Thread] Panic occurred in C-side callback function!"
                                                );
                                            }
                                        }
                                    } else {
                                        log::warn!("[FFI Callback Thread] Update channel disconnected, thread exiting.");
                                        break;
                                    }
                                },

                                () = shutdown_token.cancelled() => {
                                     log::debug!("[FFI Callback Thread] Shutdown signal received, preparing to exit.");
                                     break;
                                }
                            }
                        }
                    });
                    log::debug!(
                        "[FFI Callback Thread] Thread finished (ID: {:?}).",
                        std::thread::current().id()
                    );
                });

                handle.update_listener_handle = Some(join_handle);
            }
        } else {
            log::info!("[FFI] NULL callback passed, callback functionality is now disabled.");
        }

        SmtcResult::Success
    }));

    result.unwrap_or_else(|_| {
        log::error!("[FFI] Panic occurred inside `smtc_suite_register_update_callback`!");
        SmtcResult::InternalError
    })
}

/// Gets the current library version string.
///
/// # Returns
/// A pointer to a static UTF-8 string literal representing the library version
/// (e.g., "0.1.0"). The caller does not need to free it.
///
/// # Safety
/// Exporting this function is safe as it takes no input and returns static,
/// constant data.
#[unsafe(no_mangle)]
pub const unsafe extern "C" fn smtc_suite_get_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0")
        .as_ptr()
        .cast::<c_char>()
}

// ================================================================================================
// Command Functions
// ================================================================================================

/// (Internal) Converts the result of `try_send` to `SmtcResult`.
fn handle_try_send_result<T>(result: Result<(), mpsc::error::TrySendError<T>>) -> SmtcResult {
    match result {
        Ok(()) => SmtcResult::Success,
        Err(e) => match e {
            mpsc::error::TrySendError::Full(_) => {
                log::warn!("[FFI] Failed to send command: channel is full, please retry.");
                SmtcResult::ChannelFull
            }
            mpsc::error::TrySendError::Closed(_) => {
                log::error!("[FFI] Failed to send command: channel is closed.");
                SmtcResult::InternalError
            }
        },
    }
}

/// Requests a comprehensive status refresh.
/// This will trigger `SessionsChanged` event.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn smtc_suite_request_update(handle_ptr: *mut SmtcHandle) -> SmtcResult {
    validate_handle!(handle_ptr);
    let handle = unsafe { &*handle_ptr };

    handle
        .controller
        .as_ref()
        .map_or(SmtcResult::InvalidHandle, |controller| {
            handle_try_send_result(controller.command_tx.try_send(MediaCommand::RequestUpdate))
        })
}

/// Sends a media control command to the smtc-suite.
///
/// # Safety
/// `handle_ptr` must be a valid pointer returned by `smtc_suite_create`.
/// Exporting this function is safe as it interacts with internal state via the
/// handle and validates its inputs.
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
                    SmtcControlCommand::SetRepeatMode(RepeatMode::from(c_mode))
                }
            };
            return handle_try_send_result(
                controller
                    .command_tx
                    .try_send(MediaCommand::Control(rust_command)),
            );
        }
        SmtcResult::InvalidHandle
    }));

    res.unwrap_or(SmtcResult::InternalError)
}

/// Enables or disables high-frequency progress updates.
///
/// When enabled, the library will proactively send `TrackChanged` update events
/// at a fixed interval of 16ms to allow for smooth progress bars. When
/// disabled, `TrackChanged` events are only sent when SMTC reports a genuine
/// change.
///
/// # Arguments
/// - `handle_ptr`: A valid handle returned by `smtc_suite_create`.
/// - `enabled`: `true` to enable high-frequency updates, `false` to disable.
///
/// # Returns
/// - `SmtcResult::Success` if the command was sent successfully.
/// - `SmtcResult::InvalidHandle` if the handle is invalid.
/// - `SmtcResult::InternalError` if the command fails to send (e.g., background
///   thread is down).
///
/// # Safety
/// `handle_ptr` must be a valid pointer returned by `smtc_suite_create`.
/// Exporting this function is safe as it interacts with internal state via the
/// handle and validates its inputs.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn smtc_suite_set_high_frequency_progress_updates(
    handle_ptr: *mut SmtcHandle,
    enabled: bool,
) -> SmtcResult {
    validate_handle!(handle_ptr);
    let handle = unsafe { &*handle_ptr };

    handle
        .controller
        .as_ref()
        .map_or(SmtcResult::InvalidHandle, |controller| {
            handle_try_send_result(
                controller
                    .command_tx
                    .try_send(MediaCommand::SetHighFrequencyProgressUpdates(enabled)),
            )
        })
}

/// Selects an SMTC session for monitoring.
///
/// # Arguments
/// - `session_id`: The ID of the target session (UTF-8 encoded,
///   Null-terminated). Pass an empty string or `NULL` to switch to
///   auto-selection mode.
///
/// # Safety
/// `handle_ptr` must be valid. If `session_id` is not `NULL`, it must point to
/// a valid C string. Exporting this function is safe as it interacts with
/// internal state via the handle and validates its inputs.
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

        if let Some(controller) = &handle.controller {
            return handle_try_send_result(
                controller
                    .command_tx
                    .try_send(MediaCommand::SelectSession(session_id_str)),
            );
        }
        SmtcResult::InvalidHandle
    }));

    res.unwrap_or(SmtcResult::InternalError)
}

/// Sets the text conversion mode for SMTC metadata.
///
/// This is the function called from C to control the Simplified/Traditional
/// Chinese conversion behavior.
///
/// # Safety
///
/// The caller must ensure that `handle_ptr` is a valid pointer returned by
/// `smtc_suite_create` and has not been freed at the time this function is
/// called.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn smtc_suite_set_text_conversion_mode(
    handle_ptr: *mut SmtcHandle,
    mode: CTextConversionMode,
) -> SmtcResult {
    validate_handle!(handle_ptr);
    let handle = unsafe { &*handle_ptr };

    handle
        .controller
        .as_ref()
        .map_or(SmtcResult::InvalidHandle, |controller| {
            let rust_mode = crate::api::TextConversionMode::from(mode);
            let command = MediaCommand::SetTextConversion(rust_mode);
            handle_try_send_result(controller.command_tx.try_send(command))
        })
}

/// Requests to start audio capture.
///
/// # Safety
/// `handle_ptr` must be a valid pointer returned by `smtc_suite_create`.
/// Exporting this function is safe as it interacts with internal state via the
/// handle and validates its inputs.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn smtc_suite_start_audio_capture(handle_ptr: *mut SmtcHandle) -> SmtcResult {
    validate_handle!(handle_ptr);
    let handle = unsafe { &*handle_ptr };

    handle
        .controller
        .as_ref()
        .map_or(SmtcResult::InvalidHandle, |controller| {
            handle_try_send_result(
                controller
                    .command_tx
                    .try_send(MediaCommand::StartAudioCapture),
            )
        })
}

/// Requests to stop audio capture.
///
/// # Safety
/// `handle_ptr` must be a valid pointer returned by `smtc_suite_create`.
/// Exporting this function is safe as it interacts with internal state via the
/// handle and validates its inputs.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn smtc_suite_stop_audio_capture(handle_ptr: *mut SmtcHandle) -> SmtcResult {
    validate_handle!(handle_ptr);
    let handle = unsafe { &*handle_ptr };

    handle
        .controller
        .as_ref()
        .map_or(SmtcResult::InvalidHandle, |controller| {
            handle_try_send_result(
                controller
                    .command_tx
                    .try_send(MediaCommand::StopAudioCapture),
            )
        })
}

/// Initializes the logging system and redirects all log messages to the
/// specified C callback function.
///
/// This function should only be called once during the entire program's
/// lifetime.
///
/// # Arguments
/// - `callback`: A function pointer to receive log messages. Cannot be NULL.
/// - `userdata`: A user-defined pointer that will be passed back to the
///   callback as-is.
/// - `max_level`: The highest log level to capture.
///
/// # Returns
/// - `SmtcResult::Success`: If initialization is successful.
/// - `SmtcResult::InternalError`: If the logging system has already been
///   initialized, or another internal error occurs.
///
/// # Safety
/// `callback` must be a valid function pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn smtc_suite_init_logging(
    callback: LogCallback,
    userdata: *mut c_void,
    max_level: CLogLevel,
) -> SmtcResult {
    let logger = FfiLogger {
        state: Mutex::new(FfiLoggerState {
            callback,
            userdata: SendableVoidPtr(userdata),
        }),
    };

    if FFI_LOGGER.set(logger).is_err() {
        log::error!("[FFI] Logging system has already been initialized, cannot set it again.");
        return SmtcResult::InternalError;
    }

    if let Ok(logger_ref) = FFI_LOGGER.get().ok_or(SmtcResult::InternalError) {
        if log::set_logger(logger_ref).is_err() {
            log::error!(
                "[FFI] Failed to set global logger, it may have been occupied by another library."
            );
            return SmtcResult::InternalError;
        }
        log::set_max_level(max_level.into());
        log::info!("[FFI] Logging system initialized successfully.");
        SmtcResult::Success
    } else {
        SmtcResult::InternalError
    }
}

// ================================================================================================
// Memory Management (Internal Use Only)
// ================================================================================================

/// Frees a string (`*mut c_char`) returned by this library's FFI functions.
///
/// # Safety
/// `s` must be a valid pointer returned by one of this library's FFI functions
/// and must not have been freed yet. Passing `NULL` is safe.
fn free_string(s: *mut c_char) {
    if s.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| {
        unsafe { drop(CString::from_raw(s)) };
    }));
}

// ================================================================================================
// Internal Helper Functions & RAII Guards
// ================================================================================================

/// RAII guard to ensure `CString` is properly freed at the end of the scope
/// (including panics).
struct StringGuard(*mut c_char);
impl Drop for StringGuard {
    fn drop(&mut self) {
        free_string(self.0);
    }
}

/// RAII guard to ensure all string members of `CNowPlayingInfo` are freed.
struct NowPlayingInfoGuard(CNowPlayingInfo);
impl Drop for NowPlayingInfoGuard {
    fn drop(&mut self) {
        free_string(self.0.title.cast_mut());
        free_string(self.0.artist.cast_mut());
        free_string(self.0.album_title.cast_mut());
    }
}

/// RAII guard to ensure all string members in all `CSessionInfo` within
/// `CSessionList` are freed.
struct SessionListGuard(Vec<CSessionInfo>);
impl Drop for SessionListGuard {
    fn drop(&mut self) {
        for session in self.0.drain(..) {
            free_string(session.session_id.cast_mut());
            free_string(session.source_app_user_model_id.cast_mut());
            free_string(session.display_name.cast_mut());
        }
    }
}

/// RAII guard to ensure the string member of `CVolumeChangedEvent` is freed.
struct VolumeChangedEventGuard(CVolumeChangedEvent);
impl Drop for VolumeChangedEventGuard {
    fn drop(&mut self) {
        free_string(self.0.session_id.cast_mut());
    }
}

/// RAII guard to ensure all string members of `CDiagnosticInfo` are freed.
struct DiagnosticInfoGuard(CDiagnosticInfo);
impl Drop for DiagnosticInfoGuard {
    fn drop(&mut self) {
        free_string(self.0.message.cast_mut());
        free_string(self.0.timestamp_str.cast_mut());
    }
}

/// (Internal) Converts `MediaUpdate` to a C struct and invokes the callback.
///
/// # Safety
/// `callback` must be a valid function pointer, and `userdata` must be a valid
/// pointer.
unsafe fn process_and_invoke_callback(
    update: MediaUpdate,
    callback: UpdateCallback,
    userdata: *mut c_void,
) {
    match update {
        MediaUpdate::TrackChanged(info) => {
            let c_info = convert_to_c_now_playing_info(&info);
            let guard = NowPlayingInfoGuard(c_info);
            callback(
                CUpdateType::TrackChanged,
                (&raw const guard.0).cast::<c_void>(),
                userdata,
            );
        }
        MediaUpdate::SessionsChanged(sessions) => {
            let c_sessions: Vec<CSessionInfo> =
                sessions.iter().map(convert_to_c_session_info).collect();
            let guard = SessionListGuard(c_sessions);
            let list = CSessionList {
                sessions: guard.0.as_ptr(),
                count: guard.0.len(),
            };
            callback(
                CUpdateType::SessionsChanged,
                (&raw const list).cast::<c_void>(),
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
                (&raw const packet).cast::<c_void>(),
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
            let guard = VolumeChangedEventGuard(event);
            callback(
                CUpdateType::VolumeChanged,
                (&raw const guard.0).cast::<c_void>(),
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
        MediaUpdate::Diagnostic(info) => {
            let c_info = convert_to_c_diagnostic_info(&info);
            let guard = DiagnosticInfoGuard(c_info);
            callback(
                CUpdateType::Diagnostic,
                (&raw const guard.0).cast::<c_void>(),
                userdata,
            );
        }
    }
}

/// (Internal) Converts string slice to C `*mut c_char`.
/// If the input string contains internal NUL bytes, it will result in an empty
/// string.
fn to_c_char<S: AsRef<str>>(s: S) -> *mut c_char {
    CString::new(s.as_ref()).unwrap_or_default().into_raw()
}

/// (Internal) Converts `NowPlayingInfo` to `CNowPlayingInfo`.
fn convert_to_c_now_playing_info(info: &NowPlayingInfo) -> CNowPlayingInfo {
    CNowPlayingInfo {
        title: to_c_char(info.title.as_deref().unwrap_or("")),
        artist: to_c_char(info.artist.as_deref().unwrap_or("")),
        album_title: to_c_char(info.album_title.as_deref().unwrap_or("")),
        duration_ms: info.duration_ms.unwrap_or(0),
        position_ms: info.position_ms.unwrap_or(0),
        is_playing: info.playback_status == Some(crate::api::PlaybackStatus::Playing),
    }
}

/// (Internal) Converts `SmtcSessionInfo` to `CSessionInfo`.
fn convert_to_c_session_info(info: &SmtcSessionInfo) -> CSessionInfo {
    CSessionInfo {
        session_id: to_c_char(&info.session_id),
        source_app_user_model_id: to_c_char(&info.source_app_user_model_id),
        display_name: to_c_char(&info.display_name),
    }
}

/// (Internal) Converts `DiagnosticInfo` to `CDiagnosticInfo`.
fn convert_to_c_diagnostic_info(info: &crate::api::DiagnosticInfo) -> CDiagnosticInfo {
    let c_level = match info.level {
        crate::api::DiagnosticLevel::Warning => CDiagnosticLevel::Warning,
        crate::api::DiagnosticLevel::Error => CDiagnosticLevel::Error,
    };
    CDiagnosticInfo {
        level: c_level,
        message: to_c_char(&info.message),
        timestamp_str: to_c_char(info.timestamp.to_rfc3339()),
    }
}
