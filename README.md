# smtc-suite

[![Crates.io](https://img.shields.io/crates/v/smtc-suite.svg)](https://crates.io/crates/smtc-suite)
[![Docs.rs](https://docs.rs/smtc-suite/badge.svg)](https://docs.rs/smtc-suite)

A Rust library for interacting with Windows System Media Transport Controls (SMTC) and system audio.

`smtc-suite` provides a safe and efficient API for listening to and controlling media playback on Windows, capturing system audio output, and managing individual application volumes.

## Core Features

  * **Media Session Monitoring**: Automatically discovers all SMTC-enabled media sources in the system (like Spotify, Groove Music, etc.) and retrieves currently playing track information (title, artist, album, thumbnail).
  * **Media Playback Control**: Sends playback commands such as play, pause, skip, and seek to the currently active media session.
  * **System Audio Capture**: Captures the system's currently playing audio stream in loopback mode and provides functionality to resample it to a unified format.
  * **Independent Volume Control**: Finds the audio session for a specific application and independently gets or sets its volume.
  * **Asynchronous & Event-Driven**: All background operations are handled in a separate, efficient asynchronous worker thread, communicating with the main application via channels without blocking your app's main thread.

## Usage

The sole entry point for interacting with this library is the `MediaManager::start()` function.

1.  Calling `MediaManager::start()` launches all necessary background services and returns a tuple: `(MediaController, mpsc::Receiver<MediaUpdate>)`.
2.  The `MediaController` struct is your handle for sending commands to the background service. It contains a `command_tx` field for sending `MediaCommand`s.
3.  The `mpsc::Receiver<MediaUpdate>` is the channel through which you receive all status updates and events from the background.
4.  You can loop on this `Receiver` in a separate task to receive real-time updates.
5.  When your application exits, be sure to call `MediaController::shutdown()` or send a `MediaCommand::Shutdown` to gracefully shut down the background thread.

## Example

```rust
use smtc_suite::{MediaManager, MediaCommand, MediaUpdate};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Start the media service and get the controller and update receiver.
    let (controller, mut update_rx) = MediaManager::start()?;

    // It is recommended to handle update events from the background in a separate Tokio task.
    let update_task = tokio::spawn(async move {
        // Loop to receive updates until the channel is closed.
        while let Some(update) = update_rx.recv().await {
            match update {
                MediaUpdate::TrackChanged(info) => {
                    println!(
                        "Track Changed: {} - {}",
                        info.artist.unwrap_or_default(),
                        info.title.unwrap_or_default()
                    );
                }
                MediaUpdate::SessionsChanged(sessions) => {
                    println!("Available media session list updated, total: {}.", sessions.len());
                }
                MediaUpdate::AudioData(data) => {
                    println!("Received {} bytes of audio data.", data.len());
                }
                _ => { /* Handle other updates */ }
            }
        }
        println!("Update channel closed, event listener task exiting.");
    });

    // In the main task, we can send commands.
    // For example, wait for 5 seconds then start audio capture.
    println!("Starting audio capture in 5 seconds...");
    tokio::time::sleep(Duration::from_secs(5)).await;
    controller.command_tx.send(MediaCommand::StartAudioCapture).await?;

    // Wait for another 10 seconds.
    println!("Audio capture started, shutting down the service in 10 seconds...");
    tokio::time::sleep(Duration::from_secs(10)).await;

    // 3. Before the program exits, send the shutdown command.
    println!("Sending shutdown command...");
    controller.shutdown().await?;

    // Wait for the update task to finish.
    update_task.await?;

    println!("Program has exited gracefully.");

    Ok(())
}
```

## LICENSE

[MIT](LICENSE)