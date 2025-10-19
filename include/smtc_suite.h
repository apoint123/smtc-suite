#ifndef SMTC_SUITE_H
#define SMTC_SUITE_H

#pragma once

#include <stdarg.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>

/**
 * C-ABI compatible command type tag.
 */
typedef enum CControlCommandType {
  Pause,
  Play,
  SkipNext,
  SkipPrevious,
  SeekTo,
  SetVolume,
  SetShuffle,
  SetRepeatMode,
} CControlCommandType;

/**
 * C-ABI compatible log level enum.
 */
typedef enum CLogLevel {
  Error = 1,
  Warn = 2,
  Info = 3,
  Debug = 4,
  Trace = 5,
} CLogLevel;

/**
 * C-ABI compatible repeat mode enum.
 */
enum CRepeatMode {
  Off = 0,
  One = 1,
  All = 2,
};
typedef uint8_t CRepeatMode;

/**
 * C-ABI compatible text conversion mode enum.
 */
enum CTextConversionMode {
  /**
   * No conversion.
   */
  Off = 0,
  /**
   * Traditional to Simplified (t2s.json).
   */
  TraditionalToSimplified = 1,
  /**
   * Simplified to Traditional (s2t.json).
   */
  SimplifiedToTraditional = 2,
  /**
   * Simplified to Taiwan Standard (s2tw.json).
   */
  SimplifiedToTaiwan = 3,
  /**
   * Taiwan Standard to Simplified (tw2s.json).
   */
  TaiwanToSimplified = 4,
  /**
   * Simplified to Hong Kong Traditional (s2hk.json).
   */
  SimplifiedToHongKong = 5,
  /**
   * Hong Kong Traditional to Simplified (hk2s.json).
   */
  HongKongToSimplified = 6,
};
typedef uint8_t CTextConversionMode;

/**
 * Defines the update event types sent from Rust to C.
 */
typedef enum CUpdateType {
  /**
   * data pointer type: `*const CNowPlayingInfo` (regular update)
   */
  TrackChanged,
  /**
   * data pointer type: `*const CSessionList`
   */
  SessionsChanged,
  /**
   * data pointer type: `*const CAudioDataPacket`
   */
  AudioData,
  /**
   * data pointer type: `*const c_char` (error message string)
   */
  Error,
  /**
   * data pointer type: `*const CVolumeChangedEvent`
   */
  VolumeChanged,
  /**
   * data pointer type: `*const c_char` (ID of the vanished session)
   */
  SelectedSessionVanished,
  /**
   * data pointer type: `*const CDiagnosticInfo`
   */
  Diagnostic,
} CUpdateType;

typedef enum SmtcResult {
  Success,
  InvalidHandle,
  CreationFailed,
  InternalError,
  ChannelFull,
} SmtcResult;

/**
 * The core controller handle on the Rust side.
 */
typedef struct SmtcHandle SmtcHandle;

/**
 * Defines the callback function pointer type for receiving updates from the C
 * side.
 *
 * # Arguments
 * - `update_type`: The type of the event, used to determine how to cast the
 *   `data` pointer.
 * - `data`: A `const void*` pointer to a C struct corresponding to the event
 *   type.
 * - `userdata`: The custom context pointer passed by the caller during
 *   registration.
 */
typedef void (*UpdateCallback)(enum CUpdateType update_type, const void *data, void *userdata);

/**
 * C-ABI compatible union to hold data for different commands.
 */
typedef union ControlCommandData {
  uint64_t seek_to_ms;
  float volume_level;
  bool is_shuffle_active;
  CRepeatMode repeat_mode;
} ControlCommandData;

/**
 * C-ABI compatible control command struct.
 */
typedef struct CSmtcControlCommand {
  /**
   * The type of the command.
   */
  enum CControlCommandType command_type;
  /**
   * The data associated with the command.
   */
  union ControlCommandData data;
} CSmtcControlCommand;

/**
 * Defines the pointer type for the C side log callback function.
 *
 * # Arguments
 * - `level`: The level of the log message.
 * - `target`: The module path of the log source (e.g., "`my_lib::my_module`").
 * - `message`: The UTF-8 encoded, Null-terminated log message.
 * - `userdata`: The custom context pointer passed by the caller during
 *   registration.
 */
typedef void (*LogCallback)(enum CLogLevel level,
                            const char *target,
                            const char *message,
                            void *userdata);

/**
 * Creates a new SMTC controller instance.
 *
 * # Arguments
 * - `out_handle`: A pointer to `*mut SmtcHandle` to receive the successfully
 *   created handle.
 *
 * # Returns
 * - `SmtcResult::Success` on success, `out_handle` will be set to a valid
 *   handle.
 * - `SmtcResult::CreationFailed` on failure, `out_handle` will be set to
 *   `NULL`.
 *
 * # Safety
 * The caller is responsible for ensuring `out_handle` points to a valid memory
 * location for a `*mut SmtcHandle`. The returned handle must be freed via
 * `smtc_suite_destroy` when no longer needed to avoid resource leaks.
 * Exporting this function is safe as it has no unsafe preconditions and its
 * operation is self-contained.
 */
enum SmtcResult smtc_suite_create(struct SmtcHandle **out_handle);

/**
 * Destroys the SMTC controller instance and releases all associated resources.
 *
 * This is a safe operation, even if a `NULL` pointer is passed.
 * This function gracefully shuts down the background thread. It will block
 * synchronously to wait for the callback thread to exit, but for a maximum of
 * 5 seconds. If the callback thread does not exit within this time (e.g.,
 * blocked by a C-side callback), the function will log a warning and return,
 * which may lead to a thread resource leak.
 *
 * # Safety
 * - `handle_ptr` must be a valid pointer returned by `smtc_suite_create` that
 *   has not yet been destroyed.
 * - After calling this function, `handle_ptr` becomes an invalid (dangling)
 *   pointer and should not be used again. Exporting this function is safe as
 *   it correctly handles `NULL` input and manages the lifecycle of the
 *   resources it owns.
 */
void smtc_suite_destroy(struct SmtcHandle *handle_ptr);

/**
 * Registers a callback function for the given handle to receive all media
 * updates.
 *
 * Each call will replace the previous callback. To unregister, pass a `NULL`
 * function pointer.
 *
 * # Notes
 * - Threading Model: The callback function will be invoked on a separate
 *   background thread managed by this library. The caller must ensure that all
 *   operations within the callback are thread-safe.
 * - Data Lifetime: The `data` pointer passed to the callback (e.g.,
 *   `CNowPlayingInfo*`) is only valid for the duration of the callback's
 *   execution. If you need to retain this data, you must perform a deep copy
 *   inside the callback.
 * - Blocking Warning: The callback function should not block for long periods,
 *   as this could cause the `smtc_suite_destroy` call to time out.
 *
 * # Arguments
 * - `handle_ptr`: A valid handle returned by `smtc_suite_create`.
 * - `callback`: A function pointer to receive updates. Pass `NULL` to
 *   unregister the current callback.
 * - `userdata`: A user-defined context pointer that will be passed back to the
 *   callback as-is.
 *
 * # Safety
 * - `handle_ptr` must be a valid, non-destroyed `SmtcHandle` pointer.
 * - The caller must guarantee that the `userdata` pointer remains valid for
 *   the entire lifetime of any callback. Exporting this function is safe as it
 *   interacts with internal state via the handle and validates its inputs.
 */
enum SmtcResult smtc_suite_register_update_callback(struct SmtcHandle *handle_ptr,
                                                    UpdateCallback callback,
                                                    void *userdata);

/**
 * Gets the current library version string.
 *
 * # Returns
 * A pointer to a static UTF-8 string literal representing the library version
 * (e.g., "0.1.0"). The caller does not need to free it.
 *
 * # Safety
 * Exporting this function is safe as it takes no input and returns static,
 * constant data.
 */
const char *smtc_suite_get_version(void);

/**
 * Requests a comprehensive status refresh.
 * This will trigger `SessionsChanged` event.
 */
enum SmtcResult smtc_suite_request_update(struct SmtcHandle *handle_ptr);

/**
 * Sends a media control command to the smtc-suite.
 *
 * # Safety
 * `handle_ptr` must be a valid pointer returned by `smtc_suite_create`.
 * Exporting this function is safe as it interacts with internal state via the
 * handle and validates its inputs.
 */
enum SmtcResult smtc_suite_control_command(struct SmtcHandle *handle_ptr,
                                           struct CSmtcControlCommand command);

/**
 * Enables or disables high-frequency progress updates.
 *
 * When enabled, the library will proactively send `TrackChanged` update events
 * at a fixed interval of 100ms to allow for smooth progress bars. When
 * disabled, `TrackChanged` events are only sent when SMTC reports a genuine
 * change.
 *
 * # Arguments
 * - `handle_ptr`: A valid handle returned by `smtc_suite_create`.
 * - `enabled`: `true` to enable high-frequency updates, `false` to disable.
 *
 * # Returns
 * - `SmtcResult::Success` if the command was sent successfully.
 * - `SmtcResult::InvalidHandle` if the handle is invalid.
 * - `SmtcResult::InternalError` if the command fails to send (e.g., background
 *   thread is down).
 *
 * # Safety
 * `handle_ptr` must be a valid pointer returned by `smtc_suite_create`.
 * Exporting this function is safe as it interacts with internal state via the
 * handle and validates its inputs.
 */
enum SmtcResult smtc_suite_set_high_frequency_progress_updates(struct SmtcHandle *handle_ptr,
                                                               bool enabled);

/**
 * Selects an SMTC session for monitoring.
 *
 * # Arguments
 * - `session_id`: The ID of the target session (UTF-8 encoded,
 *   Null-terminated). Pass an empty string or `NULL` to switch to
 *   auto-selection mode.
 *
 * # Safety
 * `handle_ptr` must be valid. If `session_id` is not `NULL`, it must point to
 * a valid C string. Exporting this function is safe as it interacts with
 * internal state via the handle and validates its inputs.
 */
enum SmtcResult smtc_suite_select_session(struct SmtcHandle *handle_ptr, const char *session_id);

/**
 * Sets the text conversion mode for SMTC metadata.
 *
 * This is the function called from C to control the Simplified/Traditional
 * Chinese conversion behavior.
 *
 * # Safety
 *
 * The caller must ensure that `handle_ptr` is a valid pointer returned by
 * `smtc_suite_create` and has not been freed at the time this function is
 * called.
 */
enum SmtcResult smtc_suite_set_text_conversion_mode(struct SmtcHandle *handle_ptr,
                                                    CTextConversionMode mode);

/**
 * Requests to start audio capture.
 *
 * # Safety
 * `handle_ptr` must be a valid pointer returned by `smtc_suite_create`.
 * Exporting this function is safe as it interacts with internal state via the
 * handle and validates its inputs.
 */
enum SmtcResult smtc_suite_start_audio_capture(struct SmtcHandle *handle_ptr);

/**
 * Requests to stop audio capture.
 *
 * # Safety
 * `handle_ptr` must be a valid pointer returned by `smtc_suite_create`.
 * Exporting this function is safe as it interacts with internal state via the
 * handle and validates its inputs.
 */
enum SmtcResult smtc_suite_stop_audio_capture(struct SmtcHandle *handle_ptr);

/**
 * Initializes the logging system and redirects all log messages to the
 * specified C callback function.
 *
 * This function should only be called once during the entire program's
 * lifetime.
 *
 * # Arguments
 * - `callback`: A function pointer to receive log messages. Cannot be NULL.
 * - `userdata`: A user-defined pointer that will be passed back to the
 *   callback as-is.
 * - `max_level`: The highest log level to capture.
 *
 * # Returns
 * - `SmtcResult::Success`: If initialization is successful.
 * - `SmtcResult::InternalError`: If the logging system has already been
 *   initialized, or another internal error occurs.
 *
 * # Safety
 * `callback` must be a valid function pointer.
 */
enum SmtcResult smtc_suite_init_logging(LogCallback callback,
                                        void *userdata,
                                        enum CLogLevel max_level);

#endif  /* SMTC_SUITE_H */
