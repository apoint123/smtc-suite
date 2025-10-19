use bitflags::bitflags;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::time::Instant;
use tokio::sync::mpsc;

use crate::error::{Result, SmtcError};

bitflags! {
    /// Available control operations.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
    #[serde(transparent)]
    pub struct Controls: u8 {
        /// Playback can be started.
        const CAN_PLAY            = 1 << 0;
        /// Playback can be paused.
        const CAN_PAUSE           = 1 << 1;
        /// Can skip to the next track.
        const CAN_SKIP_NEXT       = 1 << 2;
        /// Can skip to the previous track.
        const CAN_SKIP_PREVIOUS   = 1 << 3;
        /// The playback position can be changed (seek).
        const CAN_SEEK            = 1 << 4;
        /// The shuffle mode can be changed.
        const CAN_CHANGE_SHUFFLE  = 1 << 5;
        /// The repeat mode can be changed.
        const CAN_CHANGE_REPEAT   = 1 << 6;
    }
}

/// The playback status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum PlaybackStatus {
    #[default]
    /// Playback is stopped.
    Stopped,
    /// Currently playing.
    Playing,
    /// Playback is paused.
    Paused,
}

/// Repeat mode for playback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum RepeatMode {
    #[default]
    /// Do not repeat.
    Off,
    /// Repeat the current track.
    One,
    /// Repeat the entire list.
    All,
}

/// Text conversion mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub enum TextConversionMode {
    #[default]
    /// No conversion.
    Off,
    /// Traditional to Simplified (t2s.json).
    TraditionalToSimplified,
    /// Simplified to Traditional (s2t.json).
    SimplifiedToTraditional,
    /// Simplified to Taiwan Standard (s2tw.json).
    SimplifiedToTaiwan,
    /// Taiwan Standard to Simplified (tw2s.json).
    TaiwanToSimplified,
    /// Simplified to Hong Kong Traditional (s2hk.json).
    SimplifiedToHongKong,
    /// Hong Kong Traditional to Simplified (hk2s.json).
    HongKongToSimplified,
}

/// A mutable structure used internally by the SMTC handler to
/// aggregate and manage the current player state.
#[derive(Debug, Clone, Default)]
pub struct SharedPlayerState {
    /// The song title.
    pub title: String,
    /// The artist's name.
    pub artist: String,
    /// The album title.
    pub album: String,
    /// Total duration of the song in milliseconds.
    pub song_duration_ms: u64,
    /// The last known playback position in milliseconds, as reported by SMTC.
    pub last_known_position_ms: u64,
    /// The `Instant` when `last_known_position_ms` was updated.
    pub last_known_position_report_time: Option<Instant>,
    /// The current playback status.
    pub playback_status: PlaybackStatus,
    /// The control options supported by the current media source.
    pub controls: Controls,
    /// Whether shuffle mode is currently active.
    pub is_shuffle_active: bool,
    /// The current repeat mode.
    pub repeat_mode: RepeatMode,
    /// Raw cover art data.
    pub cover_data: Option<Vec<u8>>,
    /// Hash of the cover art data.
    pub cover_data_hash: Option<u64>,
    /// A flag indicating that we are waiting for the first update from SMTC.
    /// During this time, the position timer is paused.
    pub is_waiting_for_initial_update: bool,
    /// User-defined position offset in milliseconds.
    pub position_offset_ms: i64,
    /// Optimization offset for Apple Music in milliseconds.
    pub apple_music_optimization_offset_ms: i64,
}

impl SharedPlayerState {
    /// Resets the player state to a blank/default state.
    pub fn reset_to_empty(&mut self) {
        let preserved_offset = self.position_offset_ms;
        *self = Self::default();
        self.position_offset_ms = preserved_offset;
        self.is_waiting_for_initial_update = true;
    }

    /// Estimates the real-time playback position based on
    /// the last reported position and the current time.
    pub fn get_estimated_current_position_ms(&self) -> u64 {
        let base_pos = if self.playback_status == PlaybackStatus::Playing
            && let Some(report_time) = self.last_known_position_report_time
        {
            let elapsed_ms = report_time.elapsed().as_millis() as u64;
            self.last_known_position_ms + elapsed_ms
        } else {
            self.last_known_position_ms
        };

        // Apply the user offset and the Apple Music optimization offset.
        let total_offset = self.apple_music_optimization_offset_ms - self.position_offset_ms;
        let offset_pos = (base_pos as i64 + total_offset).max(0) as u64;

        // Ensure the estimated position does not
        // exceed the total song duration (if available).
        if self.song_duration_ms > 0 {
            return offset_pos.min(self.song_duration_ms);
        }
        offset_pos
    }
}

impl From<&SharedPlayerState> for NowPlayingInfo {
    fn from(state: &SharedPlayerState) -> Self {
        Self {
            title: Some(state.title.clone()),
            artist: Some(state.artist.clone()),
            album_title: Some(state.album.clone()),
            duration_ms: Some(state.song_duration_ms),
            position_ms: Some(state.get_estimated_current_position_ms()),
            smtc_position_ms: Some(state.last_known_position_ms),
            playback_status: Some(state.playback_status),
            is_shuffle_active: Some(state.is_shuffle_active),
            repeat_mode: Some(state.repeat_mode),
            controls: Some(state.controls),
            cover_data: state.cover_data.clone(),
            cover_data_hash: state.cover_data_hash,
            position_report_time: state.last_known_position_report_time,
        }
    }
}

/// A snapshot of all information about the currently playing track.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct NowPlayingInfo {
    /// The track title.
    pub title: Option<String>,
    /// The artist's name.
    pub artist: Option<String>,
    /// The album title.
    pub album_title: Option<String>,

    /// Total duration of the track in milliseconds.
    pub duration_ms: Option<u64>,
    /// Current playback position in milliseconds. This will be an estimated
    /// value if high-frequency updates are enabled.
    pub position_ms: Option<u64>,
    /// The raw playback position in milliseconds as reported by the SMTC
    /// system, without interpolation.
    pub smtc_position_ms: Option<u64>,

    /// The current playback status.
    pub playback_status: Option<PlaybackStatus>,
    /// Whether shuffle mode is currently active.
    pub is_shuffle_active: Option<bool>,
    /// The current repeat mode.
    pub repeat_mode: Option<RepeatMode>,
    /// The control options supported by the current media source.
    pub controls: Option<Controls>,
    /// The raw byte data of the cover art.
    #[serde(skip)]
    pub cover_data: Option<Vec<u8>>,
    /// A hash of the cover art data.
    #[serde(skip)]
    pub cover_data_hash: Option<u64>,

    /// The `Instant` when the SMTC last reported the playback position.
    #[serde(skip)]
    pub position_report_time: Option<Instant>,
}

impl NowPlayingInfo {
    /// Updates self with the non-None fields from another `NowPlayingInfo`.
    pub fn update_with(&mut self, other: &Self) {
        if let Some(pos) = other.position_ms {
            self.position_ms = Some(pos);
        }
        if let Some(time) = other.position_report_time {
            self.position_report_time = Some(time);
        }
        if let Some(status) = other.playback_status {
            self.playback_status = Some(status);
        }
        if let Some(duration) = other.duration_ms {
            self.duration_ms = Some(duration);
        }
        if let Some(shuffle) = other.is_shuffle_active {
            self.is_shuffle_active = Some(shuffle);
        }
        if let Some(repeat) = other.repeat_mode {
            self.repeat_mode = Some(repeat);
        }
        if let Some(controls) = other.controls {
            self.controls = Some(controls);
        }
        if let Some(cover) = other.cover_data.clone() {
            self.cover_data = Some(cover);
            self.cover_data_hash = other.cover_data_hash;
        }
    }
}

/// Represents an available SMTC session.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct SmtcSessionInfo {
    /// A unique identifier for the session, often the same as its AUMID
    /// (Application User Model ID).
    pub session_id: String,
    /// The AUMID of the source application for the session.
    pub source_app_user_model_id: String,
    /// The name to be displayed in the UI, usually the application's
    /// executable name or a short name.
    pub display_name: String,
}

/// The control actions that can be performed on a media session.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SmtcControlCommand {
    /// Pauses playback.
    Pause,
    /// Starts or resumes playback.
    Play,
    /// Skips to the next track.
    SkipNext,
    /// Skips to the previous track.
    SkipPrevious,
    /// Seeks to a specific time point (in milliseconds).
    SeekTo(u64),
    /// Sets the volume (from 0.0 to 1.0).
    SetVolume(f32),
    /// Sets the shuffle mode.
    SetShuffle(bool),
    /// Sets the repeat mode.
    SetRepeatMode(RepeatMode),
}

/// C-ABI compatible command type tag.
#[repr(C)]
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CControlCommandType {
    Pause,
    Play,
    SkipNext,
    SkipPrevious,
    SeekTo,
    SetVolume,
    SetShuffle,
    SetRepeatMode,
}

/// C-ABI compatible repeat mode enum.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CRepeatMode {
    Off = 0,
    One = 1,
    All = 2,
}

/// C-ABI compatible union to hold data for different commands.
#[repr(C)]
#[derive(Clone, Copy)]
pub union ControlCommandData {
    pub seek_to_ms: u64,
    pub volume_level: f32,
    pub is_shuffle_active: bool,
    pub repeat_mode: CRepeatMode,
}

/// C-ABI compatible control command struct.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct CSmtcControlCommand {
    /// The type of the command.
    pub command_type: CControlCommandType,
    /// The data associated with the command.
    pub data: ControlCommandData,
}

/// C-ABI compatible text conversion mode enum.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CTextConversionMode {
    /// No conversion.
    Off = 0,
    /// Traditional to Simplified (t2s.json).
    TraditionalToSimplified = 1,
    /// Simplified to Traditional (s2t.json).
    SimplifiedToTraditional = 2,
    /// Simplified to Taiwan Standard (s2tw.json).
    SimplifiedToTaiwan = 3,
    /// Taiwan Standard to Simplified (tw2s.json).
    TaiwanToSimplified = 4,
    /// Simplified to Hong Kong Traditional (s2hk.json).
    SimplifiedToHongKong = 5,
    /// Hong Kong Traditional to Simplified (hk2s.json).
    HongKongToSimplified = 6,
}

impl From<CRepeatMode> for RepeatMode {
    fn from(c_mode: CRepeatMode) -> Self {
        match c_mode {
            CRepeatMode::Off => Self::Off,
            CRepeatMode::One => Self::One,
            CRepeatMode::All => Self::All,
        }
    }
}

impl From<CTextConversionMode> for TextConversionMode {
    fn from(c_mode: CTextConversionMode) -> Self {
        match c_mode {
            CTextConversionMode::Off => Self::Off,
            CTextConversionMode::TraditionalToSimplified => Self::TraditionalToSimplified,
            CTextConversionMode::SimplifiedToTraditional => Self::SimplifiedToTraditional,
            CTextConversionMode::SimplifiedToTaiwan => Self::SimplifiedToTaiwan,
            CTextConversionMode::TaiwanToSimplified => Self::TaiwanToSimplified,
            CTextConversionMode::SimplifiedToHongKong => Self::SimplifiedToHongKong,
            CTextConversionMode::HongKongToSimplified => Self::HongKongToSimplified,
        }
    }
}

/// Commands sent to the media library background service.
///
/// This is the primary way to interact with the media library, which is sent
/// via the `MediaController`.
#[derive(Debug, Clone)]
pub enum MediaCommand {
    /// Selects and starts listening to a specific media session.
    ///
    /// The parameter is the `session_id` of the target session.
    SelectSession(String),
    /// Executes a media control action on the currently monitored session.
    Control(SmtcControlCommand),
    /// Starts capturing system audio output.
    StartAudioCapture,
    /// Stops capturing system audio output.
    StopAudioCapture,
    /// Sets the text conversion mode for SMTC data.
    SetTextConversion(TextConversionMode),
    /// Requests the background service to immediately send a update of
    /// all key states (e.g., session list and current track).
    ///
    /// Will send `SessionsChanged` and `TrackChanged` events in response.
    RequestUpdate,
    /// Enables or disables high-frequency progress updates.
    ///
    /// When enabled, smtc-suite will actively send `TrackChanged` events at a
    /// 100ms interval to simulate a smooth progress bar.
    SetHighFrequencyProgressUpdates(bool),
    /// Sets an offset for the current playback position in milliseconds.
    SetProgressOffset(i64),
    /// Enables or disables specific optimizations for Apple Music.
    /// This only takes effect if the current session is Apple Music.
    ///
    /// Includes:
    /// 1. Splitting the album name that is sometimes merged into the artist
    ///    field (if album info is empty).
    /// 2. Applying a -500ms offset to the timeline.
    ///
    /// This optimization is enabled by default.
    SetAppleMusicOptimization(bool),
    /// Requests to shut down the entire thread.
    Shutdown,
}

/// Events and state updates received from smtc-suite.
///
/// You need to receive these updates through the `update_rx` channel of the
/// `MediaController`.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", content = "payload")]
pub enum MediaUpdate {
    /// Sent when a significant change in the media state occurs.
    ///
    /// The payload is a `NowPlayingInfo` struct, containing all the latest
    /// media information, including metadata, playback status, progress,
    /// and cover art data.
    TrackChanged(NowPlayingInfo),
    /// The list of available media sessions has been updated.
    SessionsChanged(Vec<SmtcSessionInfo>),
    /// An audio data packet has been received (if audio capture is enabled).
    AudioData(Vec<u8>),
    /// Reports a non-fatal runtime error.
    Error(String),
    /// Reports a non-fatal runtime diagnostic message.
    Diagnostic(DiagnosticInfo),
    /// The volume or mute state of a specific session has changed.
    VolumeChanged {
        /// The ID of the session that changed.
        session_id: String,
        /// The new volume level (0.0 - 1.0).
        volume: f32,
        /// The new mute state.
        is_muted: bool,
    },
    /// The previously selected session has disappeared.
    /// Often happens when the application was closed.
    SelectedSessionVanished(String),
}

/// A controller for interacting with the smtc-suite.
pub struct MediaController {
    /// The sending end of the channel for sending `MediaCommand`s to the
    /// background service.
    pub command_tx: mpsc::Sender<MediaCommand>,
}

impl MediaController {
    /// Terminates the background thread.
    pub async fn shutdown(&self) -> Result<()> {
        self.command_tx
            .send(MediaCommand::Shutdown)
            .await
            .map_err(SmtcError::from)
    }
}

/// The severity level of a diagnostic message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiagnosticLevel {
    Warning,
    Error,
}

/// Encapsulates a diagnostic message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticInfo {
    pub level: DiagnosticLevel,
    pub message: String,
    pub timestamp: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_get_estimated_current_position_ms() {
        let mut state = SharedPlayerState {
            last_known_position_ms: 10000, // 10 seconds
            song_duration_ms: 60000,       // 60 seconds
            ..Default::default()
        };

        state.playback_status = PlaybackStatus::Paused;
        state.last_known_position_report_time = Some(Instant::now());
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(
            state.get_estimated_current_position_ms(),
            10000,
            "Position should not advance when paused"
        );

        state.playback_status = PlaybackStatus::Playing;
        state.last_known_position_report_time = Some(Instant::now());
        std::thread::sleep(Duration::from_millis(500));
        let estimated_pos = state.get_estimated_current_position_ms();
        assert!(
            (10490..=10700).contains(&estimated_pos),
            "Expected position to be around 10500, but it was {estimated_pos}"
        );

        state.position_offset_ms = -2000;
        state.last_known_position_report_time = Some(Instant::now());
        std::thread::sleep(Duration::from_millis(100));
        let estimated_pos_with_offset = state.get_estimated_current_position_ms();
        assert!(
            (8090..=8300).contains(&estimated_pos_with_offset),
            "Expected position with offset to be around 8100, but it was {estimated_pos_with_offset}"
        );

        state.position_offset_ms = 3000;
        state.last_known_position_report_time = Some(Instant::now());
        let estimated_pos_with_positive_offset = state.get_estimated_current_position_ms();
        assert!(
            (12990..=13200).contains(&estimated_pos_with_positive_offset),
            "Expected position with positive offset to be around 13000, but it was {estimated_pos_with_positive_offset}"
        );

        state.position_offset_ms = 0;
        state.last_known_position_ms = 59800;
        state.last_known_position_report_time = Some(Instant::now());
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(
            state.get_estimated_current_position_ms(),
            60000,
            "Position should be capped at song duration"
        );
    }
}
