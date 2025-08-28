#ifndef SMTC_SUITE_H
#define SMTC_SUITE_H

#pragma once

#include <stdarg.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>

/**
 * C-ABI 兼容的命令类型标签。
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
 * C-ABI 兼容的日志级别枚举。
 */
typedef enum CLogLevel {
  Error = 1,
  Warn = 2,
  Info = 3,
  Debug = 4,
  Trace = 5,
} CLogLevel;

enum CRepeatMode {
  Off = 0,
  One = 1,
  All = 2,
};
typedef uint8_t CRepeatMode;

/**
 * C-ABI 兼容的文本转换模式枚举。
 *
 * 这个枚举可以安全地在 C 和 Rust 之间传递。
 */
enum CTextConversionMode {
  /**
   * 关闭转换功能。
   */
  Off = 0,
  /**
   * 繁体转简体 (t2s.json)。
   */
  TraditionalToSimplified = 1,
  /**
   * 简体转繁体 (s2t.json)。
   */
  SimplifiedToTraditional = 2,
  /**
   * 简体转台湾正体 (s2tw.json)。
   */
  SimplifiedToTaiwan = 3,
  /**
   * 台湾正体转简体 (tw2s.json)。
   */
  TaiwanToSimplified = 4,
  /**
   * 简体转香港繁体 (s2hk.json)。
   */
  SimplifiedToHongKong = 5,
  /**
   * 香港繁体转简体 (hk2s.json)。
   */
  HongKongToSimplified = 6,
};
typedef uint8_t CTextConversionMode;

/**
 * 定义从 Rust 发送到 C 的更新事件类型。
 */
typedef enum CUpdateType {
  /**
   * data 指针类型: `*const CNowPlayingInfo` (常规更新)
   */
  TrackChanged,
  /**
   * data 指针类型: `*const CSessionList`
   */
  SessionsChanged,
  /**
   * data 指针类型: `*const CAudioDataPacket`
   */
  AudioData,
  /**
   * data 指针类型: `*const c_char` (错误信息字符串)
   */
  Error,
  /**
   * data 指针类型: `*const CVolumeChangedEvent`
   */
  VolumeChanged,
  /**
   * data 指针类型: `*const c_char` (已消失会话的 ID)
   */
  SelectedSessionVanished,
  /**
   * data 指针类型: `*const CDiagnosticInfo`
   */
  Diagnostic,
} CUpdateType;

/**
 * FFI 函数的通用返回码。
 */
typedef enum SmtcResult {
  /**
   * 操作成功。
   */
  Success,
  /**
   * 传入的句柄是 NULL 或无效（例如已销毁）。
   */
  InvalidHandle,
  /**
   * 创建 SMTCS 实例失败。
   */
  CreationFailed,
  /**
   * 内部发生错误，通常伴有日志输出。
   */
  InternalError,
  /**
   * 命令因通道已满而发送失败。
   */
  ChannelFull,
} SmtcResult;

/**
 * Rust 端的核心控制器句柄。
 */
typedef struct SmtcHandle SmtcHandle;

/**
 * 定义从 C 端接收更新的回调函数指针类型。
 *
 * # 参数
 * - `update_type`: 事件的类型，用于决定如何转换 `data` 指针。
 * - `data`: 一个 `const void*` 指针，指向与事件类型对应的 C 结构体。
 * - `userdata`: 调用者在注册时传入的自定义上下文指针。
 */
typedef void (*UpdateCallback)(enum CUpdateType update_type, const void *data, void *userdata);

/**
 * C-ABI 兼容的联合体，用于存放不同命令的数据。
 */
typedef union ControlCommandData {
  uint64_t seek_to_ms;
  float volume_level;
  bool is_shuffle_active;
  CRepeatMode repeat_mode;
} ControlCommandData;

/**
 * C-ABI 兼容的、完整的控制命令结构体。
 *
 * 这个结构体可以安全地在 C 和 Rust 之间传递。
 */
typedef struct CSmtcControlCommand {
  /**
   * 命令的类型
   */
  enum CControlCommandType command_type;
  /**
   * 命令关联的数据
   */
  union ControlCommandData data;
} CSmtcControlCommand;

/**
 * 定义 C 端日志回调函数的指针类型。
 *
 * # 参数
 * - `level`: 日志消息的级别。
 * - `target`: 日志来源的模块路径 (例如 "`my_lib::my_module`")。
 * - `message`: UTF-8 编码、Null 结尾的日志消息。
 * - `userdata`: 调用者在注册时传入的自定义上下文指针。
 */
typedef void (*LogCallback)(enum CLogLevel level,
                            const char *target,
                            const char *message,
                            void *userdata);

/**
 * 创建一个新的 SMTC 控制器实例。
 *
 * # 参数
 * - `out_handle`: 一个指向 `*mut SmtcHandle` 的指针，用于接收成功创建的句柄。
 *
 * # 返回
 * - `SmtcResult::Success` 表示成功，`out_handle` 将被设置为有效的句柄。
 * - `SmtcResult::CreationFailed` 表示失败，`out_handle` 将被设置为 `NULL`。
 *
 * # 安全性
 * 调用者有责任确保 `out_handle` 指向一个有效的 `*mut SmtcHandle` 内存位置。
 * 返回的句柄必须在不再需要时通过 `smtc_suite_destroy` 释放，以避免资源泄漏。
 * 导出此函数是安全的，因为它不依赖于任何不安全的前置条件，并且其操作是独立的。
 */
enum SmtcResult smtc_suite_create(struct SmtcHandle **out_handle);

/**
 * 销毁 SMTC 控制器实例，并释放所有相关资源。
 *
 * 这是一个安全的操作，即使传入 `NULL` 指针也不会导致问题。
 * 此函数会优雅地关闭后台线程。它会同步阻塞，等待回调线程退出，
 * 但最多等待 5 秒。如果回调线程在此时间内未退出（例如被 C 端回调阻塞），
 * 函数将记录警告并返回，这可能导致线程资源泄漏。
 *
 * # 安全性
 * - `handle_ptr` 必须是一个由 `smtc_suite_create` 返回且尚未被销毁的有效指针。
 * - 在调用此函数后，`handle_ptr` 将变为无效（悬垂）指针，不应再次使用。
 *   导出此函数是安全的，因为它正确处理了 `NULL` 输入并管理其拥有的资源的生命周期。
 */
void smtc_suite_destroy(struct SmtcHandle *handle_ptr);

/**
 * 为给定的句柄注册一个回调函数，以接收所有媒体更新。
 *
 * 每次调用都会替换掉之前的回调。要注销回调，请传入一个 `NULL` 函数指针。
 *
 * # 注意
 * - **线程模型**: 回调函数将在一个由本库管理的**独立后台线程**上被调用。
 *   调用者需要确保在回调函数中的所有操作都是线程安全的。
 * - **数据生命周期**: 传递给回调函数的 `data` 指针（例如 `CNowPlayingInfo*`）
 *   **仅在回调函数的执行期间有效**。如果需要保留这些数据，必须在回调内部进行深拷贝。
 * - **阻塞警告**: 回调函数不应长时间阻塞，否则可能导致 `smtc_suite_destroy` 调用超时。
 *
 * # 参数
 * - `handle_ptr`: 一个由 `smtc_suite_create` 返回的有效句柄。
 * - `callback`: 用于接收更新的函数指针。传入 `NULL` 以注销当前的回调。
 * - `userdata`: 一个用户自定义的上下文指针，它将被原样传递给回调函数。
 *
 * # 安全性
 * - `handle_ptr` 必须是一个有效的、尚未被销毁的 `SmtcHandle` 指针。
 * - 调用者必须保证 `userdata` 指针在所有回调的生命周期内都保持有效。
 *   导出此函数是安全的，因为它通过句柄与内部状态交互，并对输入进行验证。
 */
enum SmtcResult smtc_suite_register_update_callback(struct SmtcHandle *handle_ptr,
                                                    UpdateCallback callback,
                                                    void *userdata);

/**
 * 获取当前库的版本字符串。
 *
 * # 返回
 * 一个指向静态 UTF-8 字符串的指针，表示库的版本（例如 "0.1.0"）。
 * 该指针永久有效，调用者无需释放。
 *
 * # 安全性
 * 导出此函数是安全的，因为它不接受任何输入并返回一个静态的、常量的数据。
 */
const char *smtc_suite_get_version(void);

/**
 * 请求一次全面的状态刷新。
 * 此函数会触发 `SessionsChanged` 和 `TrackChangedForced` 事件。
 */
enum SmtcResult smtc_suite_request_update(struct SmtcHandle *handle_ptr);

/**
 * 向 SMTC 套件发送一个媒体控制命令。
 *
 * # 安全性
 * `handle_ptr` 必须是一个由 `smtc_suite_create` 返回的有效指针。
 * 导出此函数是安全的，因为它通过句柄与内部状态交互，并对输入进行验证。
 */
enum SmtcResult smtc_suite_control_command(struct SmtcHandle *handle_ptr,
                                           struct CSmtcControlCommand command);

/**
 * 启用或禁用高频进度更新。
 *
 * 当启用时，库会以 100ms 的固定间隔主动发送 `TrackChanged` 更新事件，
 * 以便实现平滑的进度条。禁用后，`TrackChanged` 事件仅在 SMTC
 * 报告真实变化时才发送。
 *
 * # 参数
 * - `handle_ptr`: 一个由 `smtc_suite_create` 返回的有效句柄。
 * - `enabled`: `true` 表示启用高频更新，`false` 表示禁用。
 *
 * # 返回
 * - `SmtcResult::Success` 表示命令已成功发送。
 * - `SmtcResult::InvalidHandle` 如果句柄无效。
 * - `SmtcResult::InternalError` 如果命令发送失败（例如后台线程已关闭）。
 *
 * # 安全性
 * `handle_ptr` 必须是一个由 `smtc_suite_create` 返回的有效指针。
 * 导出此函数是安全的，因为它通过句柄与内部状态交互，并对输入进行验证。
 */
enum SmtcResult smtc_suite_set_high_frequency_progress_updates(struct SmtcHandle *handle_ptr,
                                                               bool enabled);

/**
 * 选择一个 SMTC 会话进行监控。
 *
 * # 参数
 * - `session_id`: 目标会话的 ID (UTF-8 编码, Null 结尾)。传入空字符串或 `NULL` 以切换到自动选择模式。
 *
 * # 安全性
 * `handle_ptr` 必须有效。如果 `session_id` 非 `NULL`，它必须指向一个有效的 C 字符串。
 * 导出此函数是安全的，因为它通过句柄与内部状态交互，并对输入进行验证。
 */
enum SmtcResult smtc_suite_select_session(struct SmtcHandle *handle_ptr,
                                          const char *session_id);

/**
 * 设置 SMTC 元数据的文本转换模式。
 *
 * 这是从 C 语言调用以控制简繁转换行为的函数。
 *
 * # 安全性
 *
 * 调用者必须确保 `controller_ptr` 是一个由 `smtc_start` 返回的有效指针，
 * 并且在调用此函数时没有被释放。
 */
enum SmtcResult smtc_suite_set_text_conversion_mode(struct SmtcHandle *handle_ptr,
                                                    CTextConversionMode mode);

/**
 * 请求开始音频捕获。
 *
 * # 安全性
 * `handle_ptr` 必须是一个由 `smtc_suite_create` 返回的有效指针。
 * 导出此函数是安全的，因为它通过句柄与内部状态交互，并对输入进行验证。
 */
enum SmtcResult smtc_suite_start_audio_capture(struct SmtcHandle *handle_ptr);

/**
 * 请求停止音频捕获。
 *
 * # 安全性
 * `handle_ptr` 必须是一个由 `smtc_suite_create` 返回的有效指针。
 * 导出此函数是安全的，因为它通过句柄与内部状态交互，并对输入进行验证。
 */
enum SmtcResult smtc_suite_stop_audio_capture(struct SmtcHandle *handle_ptr);

/**
 * 初始化日志系统，并将所有日志消息重定向到指定的 C 回调函数。
 *
 * 这个函数在整个程序的生命周期中只应被调用一次。
 *
 * # 参数
 * - `callback`: 用于接收日志消息的函数指针。不能为 NULL。
 * - `userdata`: 将被原样传递给回调函数的用户自定义指针。
 * - `max_level`: 要捕获的最高日志级别。
 *
 * # 返回
 * - `SmtcResult::Success`: 如果成功初始化。
 * - `SmtcResult::InternalError`: 如果日志系统已经被初始化过，或者发生其他内部错误。
 *
 * # 安全性
 * 必须保证 `callback` 是一个有效的指针。
 */
enum SmtcResult smtc_suite_init_logging(LogCallback callback,
                                        void *userdata,
                                        enum CLogLevel max_level);

#endif  /* SMTC_SUITE_H */
