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
//!   通过通道与主应用通信，不会阻塞你的应用主线程。
//!
//! ## 使用方法
//!
//! 与本库交互的唯一入口是 [`MediaManager::start()`] 函数。
//!
//! 1.  调用 `MediaManager::start()` 会启动所有必需的后台服务，并返回一个元组：
//!     `(MediaController, mpsc::Receiver<MediaUpdate>)`。
//! 2.  [`MediaController`] 结构体是你向后台服务发送指令的句柄。它包含一个 `command_tx`
//!     字段，用于发送 [`MediaCommand`]。
//! 3.  `mpsc::Receiver<MediaUpdate>` 是你接收所有来自后台的状态更新和事件的通道。
//! 4.  你可以在一个独立的任务中循环监听这个 `Receiver` 以接收实时更新。
//! 5.  当你的应用退出时，务必调用 [`MediaController::shutdown()`] 或发送一个 `MediaCommand::Shutdown`
//!     来优雅地关闭后台线程。
//!
//! ## 示例
//!
//! ```no_run
//! use smtc_suite::{MediaManager, MediaCommand, MediaUpdate};
//! use std::time::Duration;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // 1. 启动媒体服务并获取控制器和更新接收器
//!     let (controller, mut update_rx) = MediaManager::start()?;
//!
//!     // 推荐在一个单独的 Tokio 任务中处理来自后台的更新事件
//!     let update_task = tokio::spawn(async move {
//!         // 循环接收更新，直到通道关闭
//!         while let Some(update) = update_rx.recv().await {
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
//!         println!("更新通道已关闭，事件监听任务退出。");
//!     });
//!
//!     // 在主任务中，我们可以发送命令
//!     // 例如，等待5秒后启动音频捕获
//!     println!("将在5秒后启动音频捕获...");
//!     tokio::time::sleep(Duration::from_secs(5)).await;
//!     controller.command_tx.send(MediaCommand::StartAudioCapture).await?;
//!
//!     // 再等待10秒
//!     println!("音频捕获已启动，将在10秒后关闭服务...");
//!     tokio::time::sleep(Duration::from_secs(10)).await;
//!
//!     // 3. 在程序退出前，发送关闭命令
//!     println!("正在发送关闭命令...");
//!     controller.shutdown().await?;
//!
//!     // 等待更新任务结束
//!     update_task.await?;
//!
//!     println!("程序已优雅退出。");
//!
//!     Ok(())
//! }
//! ```

mod api;
mod audio_capture;
mod audio_session_monitor;
mod error;
mod ffi;
mod smtc_handler;
mod tasks;
mod utils;
mod volume_control;
mod worker;

pub use api::{
    Controls, MediaCommand, MediaController, MediaUpdate, NowPlayingInfo, PlaybackStatus,
    RepeatMode, SmtcControlCommand, SmtcSessionInfo, TextConversionMode,
};
pub use error::{Result, SmtcError};

use std::sync::{LazyLock, Mutex};
use std::thread::JoinHandle;
use tokio::sync::mpsc;

static WORKER_HANDLE: LazyLock<Mutex<Option<JoinHandle<()>>>> = LazyLock::new(|| Mutex::new(None));

/// `MediaManager` 是本库的静态入口点。
pub struct MediaManager;

impl MediaManager {
    /// 启动所有后台监控服务，并返回一个控制器和事件接收器。
    ///
    /// 这是与本库交互的唯一正确方式。它会初始化并运行一个专用的后台工作线程，
    /// 该线程负责处理所有与系统 API 的交互。
    ///
    /// # 返回
    /// - `Ok((controller, update_rx))`: 成功启动后，返回一个元组：
    ///   - `controller`: 一个 [`MediaController`]，用于向后台服务发送命令。
    ///   - `update_rx`: 一个 `mpsc::Receiver<MediaUpdate>`，用于接收所有事件和状态更新。
    /// - `Err(SmtcError)`: 如果在启动过程中发生严重错误，或服务已在运行。
    pub fn start() -> Result<(MediaController, mpsc::Receiver<MediaUpdate>)> {
        {
            let handle_guard = WORKER_HANDLE.lock()?;
            if let Some(handle) = handle_guard.as_ref()
                && !handle.is_finished()
            {
                return Err(SmtcError::AlreadyRunning);
            }
        }

        let (command_tx, command_rx) = mpsc::channel::<MediaCommand>(32);
        let (update_tx, update_rx) = mpsc::channel::<MediaUpdate>(32);

        let new_handle = worker::start_media_worker_thread(command_rx, update_tx)?;

        let controller = MediaController { command_tx };

        {
            let mut handle_guard = WORKER_HANDLE.lock()?;
            *handle_guard = Some(new_handle);
        }

        Ok((controller, update_rx))
    }
}
