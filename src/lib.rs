#![warn(missing_docs)]

//! 一个用于与 Windows 系统媒体控件 (SMTC) 和系统音频进行交互的 Rust 库。
//!
//! `smtc-suite` 提供了一套安全、高效的 API，用于监听和控制 Windows 上的媒体播放、
//! 捕获系统音频输出，以及管理单个应用的音量。
//!
//! ## 核心功能
//!
//! * **媒体会话监控**: 自动发现系统中所有支持 SMTC 的媒体源（如 Spotify, Groove 音乐等），
//!   并获取当前播放的曲目信息（标题、艺术家、专辑、封面）。
//! * **媒体播放控制**: 对当前活动的媒体会话发送播放、暂停、切歌、跳转等控制命令。
//! * **系统音频捕获**: 以环回模式捕获系统正在播放的音频流，并提供重采样到统一格式的功能。
//! * **独立音量控制**: 查找特定应用的音频会话，并独立获取或设置其音量。
//! * **异步事件驱动**: 所有后台操作都在一个独立的、高效的异步工作线程中进行，
//!   通过通道与主应用通信，不会阻塞您的应用主线程。
//!
//! ## 使用方法
//!
//! 与本库交互的唯一入口是 [`MediaManager::start()`] 函数。
//!
//! 1.  调用 `MediaManager::start()` 会启动所有必需的后台服务，并返回一个 [`Result<MediaController>`]。
//! 2.  [`MediaController`] 结构体是您与后台服务交互的句柄。它包含两个字段：
//!     * `command_tx`: 一个 `Sender<MediaCommand>`，用于向后台发送指令。
//!     * `update_rx`: 一个 `Receiver<MediaUpdate>`，用于接收来自后台的状态更新和事件。
//! 3.  您可以在一个独立的线程中循环监听 `update_rx` 以接收实时更新。
//! 4.  通过 `command_tx` 发送 [`MediaCommand`] 枚举中的指令来控制后台服务。
//! 5.  当您的应用退出时，务必调用 [`MediaController::shutdown()`] 或发送一个 `MediaCommand::Shutdown`
//!     来优雅地关闭后台线程。
//!
//! ## 示例
//!
//! ```no_run
//! use smtc_suite::{MediaManager, MediaCommand, MediaUpdate};
//! use std::thread;
//! use std::time::Duration;
//!
//! fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // 1. 启动媒体服务并获取控制器
//!     let controller = MediaManager::start()?;
//!
//!     // 推荐在一个单独的线程中处理来自后台的更新事件
//!     let update_receiver = controller.update_rx;
//!     let update_thread = thread::spawn(move || {
//!         // 循环接收更新，直到通道关闭
//!         while let Ok(update) = update_receiver.recv() {
//!             match update {
//!                 MediaUpdate::TrackChanged(info) => {
//!                     println!(
//!                         "曲目变更: {} - {}",
//!                         info.artist.unwrap_or_default(),
//!                         info.title.unwrap_or_default()
//!                     );
//!                 }
//!                 MediaUpdate::SessionsChanged(sessions) => {
//!                     println!("可用媒体会话列表已更新，共 {} 个。", sessions.len());
//!                 }
//!                 MediaUpdate::AudioData(data) => {
//!                     println!("收到 {} 字节的音频数据。", data.len());
//!                 }
//!                 _ => { /* 处理其他更新 */ }
//!             }
//!         }
//!         println!("更新通道已关闭，事件监听线程退出。");
//!     });
//!
//!     // 在主线程中，我们可以发送命令
//!     // 例如，等待5秒后启动音频捕获
//!     println!("将在5秒后启动音频捕获...");
//!     thread::sleep(Duration::from_secs(5));
//!     controller.command_tx.send(MediaCommand::StartAudioCapture)?;
//!
//!     // 再等待10秒
//!     println!("音频捕获已启动，将在10秒后关闭服务...");
//!     thread::sleep(Duration::from_secs(10));
//!
//!     // 3. 在程序退出前，发送关闭命令
//!     println!("正在发送关闭命令...");
//!     controller.shutdown()?;
//!
//!     // 等待更新线程结束
//!     update_thread.join().expect("更新线程 join 失败");
//!
//!     println!("程序已优雅退出。");
//!
//!     Ok(())
//! }
//! ```

mod api;
mod audio_capture;
mod error;
mod ffi;
mod smtc_handler;
mod utils;
mod volume_control;
mod worker;

pub use api::{
    MediaCommand, MediaController, MediaUpdate, NowPlayingInfo, RepeatMode, SmtcControlCommand,
    SmtcSessionInfo, TextConversionMode,
};
pub use error::{Result, SmtcError};

/// `MediaManager` 是本库的静态入口点。
pub struct MediaManager;

impl MediaManager {
    /// 启动所有后台监控服务，并返回一个控制器。
    ///
    /// 这是与本库交互的唯一正确方式。它会初始化并运行一个专用的后台工作线程，
    /// 该线程负责处理所有与系统 API 的交互。
    ///
    /// # 返回
    /// - `Ok(MediaController)`: 成功启动后，返回一个控制器用于后续的交互。
    /// - `Err(SmtcError)`: 如果在启动过程中发生严重错误（例如，无法创建后台线程或 Tokio 运行时）。
    pub fn start() -> Result<MediaController> {
        let (command_tx, command_rx) = crossbeam_channel::unbounded::<MediaCommand>();
        let (update_tx, update_rx) = crossbeam_channel::unbounded::<MediaUpdate>();

        // 启动后台协调器线程
        worker::start_media_worker_thread(command_rx, update_tx)?;

        Ok(MediaController {
            command_tx,
            update_rx,
        })
    }
}
