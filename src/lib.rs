//! A Rust library for interacting with Windows System Media Transport Controls
//! (SMTC) and system audio.
//!
//! `smtc-suite` provides a safe and efficient API for listening to and
//! controlling media playback on Windows, capturing system audio output, and
//! managing individual application volumes.
//!
//! ## Features
//!
//! * Media Session Monitoring: Automatically discovers all SMTC-enabled media
//!   sources in the system (like Spotify, Groove Music, etc.) and retrieves
//!   currently playing track information (title, artist, album, thumbnail).
//! * Media Playback Control: Sends playback commands such as play, pause, skip,
//!   and seek to the currently active media session.
//! * Audio Capture: Captures the system's currently playing audio stream in
//!   loopback mode and provides functionality to resample it to a unified
//!   format.
//! * Volume Control: Finds the audio session for a specific application and
//!   gets or sets its volume.
//! * Asynchronous & Event-Driven: All background operations are handled in a
//!   separate, asynchronous worker thread, communicating with the main
//!   application via channels without blocking your app's main thread.
//!
//! ## Usage
//!
//! The sole entry point for interacting with this library is the
//! [`MediaManager::start()`] function.
//!
//! 1. Calling `MediaManager::start()` launches all necessary background
//!    services and returns a tuple: `(MediaController,
//!    mpsc::Receiver<MediaUpdate>)`.
//! 2. The [`MediaController`] struct is your handle for sending commands to the
//!    background service. It contains a `command_tx` field for sending
//!    [`MediaCommand`]s.
//! 3. The `mpsc::Receiver<MediaUpdate>` is the channel through which you
//!    receive all status updates and events from the background.
//! 4. You can loop on this `Receiver` in a separate task to receive real-time
//!    updates.
//! 5. When your application exits, be sure to call
//!    [`MediaController::shutdown()`] or send a `MediaCommand::Shutdown` to
//!    gracefully shut down the background thread.
//!
//! ## Example
//!
//! ```no_run
//! use smtc_suite::{MediaManager, MediaCommand, MediaUpdate};
//! use std::time::Duration;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // 1. Start the media service and get the controller and update receiver.
//!     let (controller, mut update_rx) = MediaManager::start()?;
//!
//!     // It is recommended to handle update events from the background in a separate Tokio task.
//!     let update_task = tokio::spawn(async move {
//!         // Loop to receive updates until the channel is closed.
//!         while let Some(update) = update_rx.recv().await {
//!             match update {
//!                 MediaUpdate::TrackChanged(info) => {
//!                     println!(
//!                         "Track Changed: {} - {}",
//!                         info.artist.unwrap_or_default(),
//!                         info.title.unwrap_or_default()
//!                     );
//!                 }
//!                 MediaUpdate::SessionsChanged(sessions) => {
//!                     println!("Available media session list updated, total: {}.", sessions.len());
//!                 }
//!                 MediaUpdate::AudioData(data) => {
//!                     println!("Received {} bytes of audio data.", data.len());
//!                 }
//!                 _ => { /* Handle other updates */ }
//!             }
//!         }
//!         println!("Update channel closed, event listener task exiting.");
//!     });
//!
//!     // In the main task, we can send commands.
//!     // For example, wait for 5 seconds then start audio capture.
//!     println!("Starting audio capture in 5 seconds...");
//!     tokio::time::sleep(Duration::from_secs(5)).await;
//!     controller.command_tx.send(MediaCommand::StartAudioCapture).await?;
//!
//!     // Wait for another 10 seconds.
//!     println!("Audio capture started, shutting down the service in 10 seconds...");
//!     tokio::time::sleep(Duration::from_secs(10)).await;
//!
//!     // 3. Before the program exits, send the shutdown command.
//!     println!("Sending shutdown command...");
//!     controller.shutdown().await?;
//!
//!     // Wait for the update task to finish.
//!     update_task.await?;
//!
//!     println!("Program has exited gracefully.");
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
    Controls, MediaCommand, MediaController, MediaType, MediaUpdate, NowPlayingInfo,
    PlaybackStatus, RepeatMode, SmtcControlCommand, SmtcSessionInfo, TextConversionMode,
};
pub use error::{Result, SmtcError};

use std::sync::{LazyLock, Mutex};
use std::thread::JoinHandle;
use tokio::sync::mpsc;

static WORKER_HANDLE: LazyLock<Mutex<Option<JoinHandle<()>>>> = LazyLock::new(|| Mutex::new(None));

/// `MediaManager` is the entry point for this library.
pub struct MediaManager;

impl MediaManager {
    /// Starts all background monitoring services and returns a controller and
    /// an event receiver.
    ///
    /// # Returns
    /// - `Ok((controller, update_rx))`: On successful startup, returns a tuple:
    ///   - `controller`: A [`MediaController`] for sending commands to the
    ///     background service.
    ///   - `update_rx`: An `mpsc::Receiver<MediaUpdate>` for receiving all
    ///     events and status updates.
    /// - `Err(SmtcError)`: If a critical error occurs during startup, or if the
    ///   service is already running.
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
