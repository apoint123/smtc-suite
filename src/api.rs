use bitflags::bitflags;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::time::Instant;
use tokio::sync::mpsc;

use crate::error::{Result, SmtcError};

bitflags! {
    /// 当前可用的控制操作
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
    #[serde(transparent)]
    pub struct Controls: u8 {
        /// 是否可以播放
        const CAN_PLAY            = 1 << 0;
        /// 是否可以暂停
        const CAN_PAUSE           = 1 << 1;
        /// 是否可以跳到下一首
        const CAN_SKIP_NEXT       = 1 << 2;
        /// 是否可以跳到上一首
        const CAN_SKIP_PREVIOUS   = 1 << 3;
        /// 是否可以跳转进度
        const CAN_SEEK            = 1 << 4;
        /// 是否可以改变随机播放模式
        const CAN_CHANGE_SHUFFLE  = 1 << 5;
        /// 是否可以改变重复播放模式
        const CAN_CHANGE_REPEAT   = 1 << 6;
    }
}

/// 播放状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum PlaybackStatus {
    #[default]
    /// 已停止
    Stopped,
    /// 播放中
    Playing,
    /// 已暂停
    Paused,
}

/// 定义重复播放模式的枚举。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum RepeatMode {
    #[default]
    /// 不重复播放。
    Off,
    /// 单曲循环。
    One,
    /// 歌单循环。
    All,
}

/// 定义文本转换模式的枚举。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub enum TextConversionMode {
    #[default]
    /// 关闭转换功能。
    Off,
    /// 繁体转简体 (t2s.json)。
    TraditionalToSimplified,
    /// 简体转繁体 (s2t.json)。
    SimplifiedToTraditional,
    /// 简体转台湾正体 (s2tw.json)。
    SimplifiedToTaiwan,
    /// 台湾正体转简体 (tw2s.json)。
    TaiwanToSimplified,
    /// 简体转香港繁体 (s2hk.json)。
    SimplifiedToHongKong,
    /// 香港繁体转简体 (hk2s.json)。
    HongKongToSimplified,
}

/// SMTC Handler 内部用于聚合和管理当前播放状态的可变结构。
#[derive(Debug, Clone, Default)]
pub struct SharedPlayerState {
    /// 歌曲标题。
    pub title: String,
    /// 艺术家。
    pub artist: String,
    /// 专辑。
    pub album: String,
    /// 歌曲总时长（毫秒）。
    pub song_duration_ms: u64,
    /// 上次从 SMTC 获取到的播放位置（毫秒）。
    pub last_known_position_ms: u64,
    /// `last_known_position_ms` 被更新时的时间点。
    pub last_known_position_report_time: Option<Instant>,
    /// 当前的播放状态。
    pub playback_status: PlaybackStatus,
    /// 当前媒体源支持的控制选项。
    pub controls: Controls,
    /// 当前是否处于随机播放模式。
    pub is_shuffle_active: bool,
    /// 当前的重复播放模式。
    pub repeat_mode: RepeatMode,
    pub cover_data: Option<Vec<u8>>,
    pub cover_data_hash: Option<u64>,
    /// 一个标志，表示正在等待来自SMTC的第一次更新。
    /// 在此期间，应暂停进度计时器。
    pub is_waiting_for_initial_update: bool,
    pub position_offset_ms: i64,
    pub apple_music_optimization_offset_ms: i64,
}

impl SharedPlayerState {
    /// 将播放状态重置为空白/默认状态。
    /// 通常在没有活动媒体会话时调用。
    pub fn reset_to_empty(&mut self) {
        *self = Self::default();
        self.is_waiting_for_initial_update = true;
    }

    /// 根据上次报告的播放位置和当前时间，估算实时的播放进度。
    pub fn get_estimated_current_position_ms(&self) -> u64 {
        let base_pos = if self.playback_status == PlaybackStatus::Playing
            && let Some(report_time) = self.last_known_position_report_time
        {
            let elapsed_ms = report_time.elapsed().as_millis() as u64;
            self.last_known_position_ms + elapsed_ms
        } else {
            self.last_known_position_ms
        };

        // 叠加用户偏移和 Apple Music 的优化偏移
        let total_offset = self.apple_music_optimization_offset_ms - self.position_offset_ms;
        let offset_pos = (base_pos as i64 + total_offset).max(0) as u64;

        // 确保估算的位置不超过歌曲总时长（如果时长有效）
        if self.song_duration_ms > 0 {
            return offset_pos.min(self.song_duration_ms);
        }
        offset_pos
    }
}

/// 实现从内部共享状态到公共 `NowPlayingInfo` 的转换。
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

/// 包含当前正在播放曲目所有信息的、唯一的、完整的状态快照。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct NowPlayingInfo {
    /// 曲目标题。
    pub title: Option<String>,
    /// 艺术家名称。
    pub artist: Option<String>,
    /// 专辑标题。
    pub album_title: Option<String>,

    /// 曲目总时长（毫秒）。
    pub duration_ms: Option<u64>,
    /// 当前播放位置（毫秒）。如果启用了高频更新，这将是一个估算值。
    pub position_ms: Option<u64>,
    /// SMTC 系统报告的原始播放位置（毫秒），未经插值。
    pub smtc_position_ms: Option<u64>,

    /// 当前的播放状态。
    pub playback_status: Option<PlaybackStatus>,
    /// 当前是否处于随机播放模式。
    pub is_shuffle_active: Option<bool>,
    /// 当前的重复播放模式。
    pub repeat_mode: Option<RepeatMode>,
    /// 当前媒体源支持的控制选项。
    pub controls: Option<Controls>,
    /// 封面图片的原始字节数据 (`Vec<u8>`)。
    #[serde(skip)]
    pub cover_data: Option<Vec<u8>>,
    /// 封面数据的哈希值。
    #[serde(skip)]
    pub cover_data_hash: Option<u64>,

    /// SMTC 上次报告播放位置的时间点。
    #[serde(skip)]
    pub position_report_time: Option<Instant>,
}

impl NowPlayingInfo {
    /// 用另一份 `NowPlayingInfo` 的非 None 字段更新自身
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

/// 表示一个可用的系统媒体传输控件 (SMTC) 会话。
///
/// 每个支持媒体控制的应用（如 Spotify, Windows Media Player）都会创建一个会话。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct SmtcSessionInfo {
    /// 会话的唯一标识符，通常与其 AUMID (Application User Model ID) 相同。
    pub session_id: String,
    /// 会话来源应用的 AUMID。
    pub source_app_user_model_id: String,
    /// 用于在 UI 中显示的名称，通常是应用的可执行文件名或简称。
    pub display_name: String,
}

/// 定义了可以对媒体会话执行的控制操作。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SmtcControlCommand {
    /// 暂停播放。
    Pause,
    /// 开始或恢复播放。
    Play,
    /// 跳到下一首。
    SkipNext,
    /// 跳到上一首。
    SkipPrevious,
    /// 跳转到指定的时间点（毫秒）。
    SeekTo(u64),
    /// 设置音量（0.0 到 1.0）。
    SetVolume(f32),
    /// 设置随机播放模式。
    SetShuffle(bool),
    /// 设置重复播放模式。
    SetRepeatMode(RepeatMode),
}

/// C-ABI 兼容的命令类型标签。
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

// C-ABI 兼容的重复模式枚举。
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CRepeatMode {
    Off = 0,
    One = 1,
    All = 2,
}

/// C-ABI 兼容的联合体，用于存放不同命令的数据。
#[repr(C)]
#[derive(Clone, Copy)]
pub union ControlCommandData {
    pub seek_to_ms: u64,
    pub volume_level: f32,
    pub is_shuffle_active: bool,
    pub repeat_mode: CRepeatMode,
}

/// C-ABI 兼容的、完整的控制命令结构体。
///
/// 这个结构体可以安全地在 C 和 Rust 之间传递。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct CSmtcControlCommand {
    /// 命令的类型
    pub command_type: CControlCommandType,
    /// 命令关联的数据
    pub data: ControlCommandData,
}

/// C-ABI 兼容的文本转换模式枚举。
///
/// 这个枚举可以安全地在 C 和 Rust 之间传递。
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CTextConversionMode {
    /// 关闭转换功能。
    Off = 0,
    /// 繁体转简体 (t2s.json)。
    TraditionalToSimplified = 1,
    /// 简体转繁体 (s2t.json)。
    SimplifiedToTraditional = 2,
    /// 简体转台湾正体 (s2tw.json)。
    SimplifiedToTaiwan = 3,
    /// 台湾正体转简体 (tw2s.json)。
    TaiwanToSimplified = 4,
    /// 简体转香港繁体 (s2hk.json)。
    SimplifiedToHongKong = 5,
    /// 香港繁体转简体 (hk2s.json)。
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

/// 发送给媒体库后台服务的命令。
///
/// 这是与媒体库交互的主要方式，由 `MediaController` 发送。
#[derive(Debug, Clone)]
pub enum MediaCommand {
    /// 选择并开始监听指定的媒体会话。
    ///
    /// 参数是目标会话的 `session_id`。
    SelectSession(String),
    /// 对当前监听的会话执行一个媒体控制操作。
    Control(SmtcControlCommand),
    /// 开始捕获系统音频输出。
    StartAudioCapture,
    /// 停止捕获系统音频输出。
    StopAudioCapture,
    /// 设置 SMTC 数据的文本转换模式。
    SetTextConversion(TextConversionMode),
    /// 请求后台服务立即发送一次所有关键状态的更新（例如会话列表和当前曲目）。
    ///
    /// 后台服务将异步地发送 `SessionsChanged` 和 `TrackChanged` 事件作为响应。
    RequestUpdate,
    /// 启用或禁用高频进度更新。
    ///
    /// 当启用时，smtc-suite 会以 100ms 的频率主动发送 `TrackChanged` 事件来模拟平滑进度。
    SetHighFrequencyProgressUpdates(bool),
    /// 为当前播放进度设置一个偏移量（毫秒）。
    SetProgressOffset(i64),
    /// 启用或禁用针对 Apple Music 的特定优化。
    /// 仅在当前的会话是 Apple Music 时才有效果。
    ///
    ///
    /// 包括：
    /// 1. 拆分合并在艺术家字段中的专辑名。(如果专辑信息为空)
    /// 2. 为时间轴应用 -500ms 的偏移。
    ///
    /// 此优化默认启用。
    SetAppleMusicOptimization(bool),
    /// 请求关闭整个媒体服务后台线程。
    Shutdown,
}

/// 从媒体库后台服务接收的事件和状态更新。
///
/// 使用者通过 `MediaController` 的 `update_rx` 通道接收这些更新。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", content = "payload")]
pub enum MediaUpdate {
    /// 当媒体状态发生有意义的变化时发送。
    ///
    /// 负载是一个 `NowPlayingInfo` 结构体，包含了所有最新的媒体信息，
    /// 包括元数据、播放状态、进度和封面数据。
    TrackChanged(NowPlayingInfo),
    /// 可用的媒体会话列表已更新。
    SessionsChanged(Vec<SmtcSessionInfo>),
    /// 接收到一个音频数据包（如果音频捕获已启动）。
    AudioData(Vec<u8>),
    /// 报告一个非致命的运行时错误。
    Error(String),
    /// 报告一个非致命的运行时诊断信息。
    Diagnostic(DiagnosticInfo),
    /// 指定会话的音量或静音状态已发生变化。
    VolumeChanged {
        /// 发生变化的会话 ID。
        session_id: String,
        /// 新的音量级别 (0.0 - 1.0)。
        volume: f32,
        /// 新的静音状态。
        is_muted: bool,
    },
    /// 指示之前用户选中的会话已经消失（例如，应用已关闭）。
    SelectedSessionVanished(String),
}

/// 与后台服务交互的控制器。
pub struct MediaController {
    /// 用于向后台服务发送 `MediaCommand` 的通道发送端。
    pub command_tx: mpsc::Sender<MediaCommand>,
}

impl MediaController {
    /// 终止后台线程。
    pub async fn shutdown(&self) -> Result<()> {
        self.command_tx
            .send(MediaCommand::Shutdown)
            .await
            .map_err(SmtcError::from)
    }
}

/// 诊断信息的严重级别
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiagnosticLevel {
    Warning,
    Error,
}

/// 封装一条诊断信息
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
