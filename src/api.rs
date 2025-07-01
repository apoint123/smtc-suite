use crossbeam_channel::{self, Receiver, SendError, Sender};
use serde::{Deserialize, Serialize};
use std::time::Instant;

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

/// 包含当前正在播放曲目所有信息的、唯一的、完整的状态快照。
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct NowPlayingInfo {
    /// 曲目标题。
    pub title: Option<String>,
    /// 艺术家名称。
    pub artist: Option<String>,
    /// 专辑标题。
    pub album_title: Option<String>,

    /// 曲目总时长（毫秒）。
    pub duration_ms: Option<u64>,
    /// 当前播放位置（毫秒）。
    ///
    /// 这是一个估算值，结合了 SMTC 上次报告的位置和自那时起经过的时间，
    /// 以便在没有高频事件时也能提供相对平滑的进度。
    pub position_ms: Option<u64>,

    /// 当前是否正在播放。
    pub is_playing: Option<bool>,
    /// 当前是否处于随机播放模式。
    pub is_shuffle_active: Option<bool>,
    /// 当前的重复播放模式。
    pub repeat_mode: Option<RepeatMode>,

    /// 指示媒体源当前是否允许“播放”操作。
    pub can_play: Option<bool>,
    /// 指示媒体源当前是否允许“暂停”操作。
    pub can_pause: Option<bool>,
    /// 指示媒体源当前是否允许“下一首”操作。
    pub can_skip_next: Option<bool>,
    /// 指示媒体源当前是否允许“上一首”操作。
    pub can_skip_previous: Option<bool>,

    /// 封面图片的原始字节数据 (`Vec<u8>`)。
    #[serde(skip)]
    pub cover_data: Option<Vec<u8>>,
    /// 封面数据的哈希值。
    ///
    /// 可用于快速比较封面是否已更改。
    #[serde(skip)]
    pub cover_data_hash: Option<u64>,

    /// (内部使用) SMTC 上次报告播放位置的时间点。
    #[serde(skip)]
    pub position_report_time: Option<Instant>,
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
#[allow(dead_code)]
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

impl From<CTextConversionMode> for TextConversionMode {
    fn from(c_mode: CTextConversionMode) -> Self {
        match c_mode {
            CTextConversionMode::Off => TextConversionMode::Off,
            CTextConversionMode::TraditionalToSimplified => {
                TextConversionMode::TraditionalToSimplified
            }
            CTextConversionMode::SimplifiedToTraditional => {
                TextConversionMode::SimplifiedToTraditional
            }
            CTextConversionMode::SimplifiedToTaiwan => TextConversionMode::SimplifiedToTaiwan,
            CTextConversionMode::TaiwanToSimplified => TextConversionMode::TaiwanToSimplified,
            CTextConversionMode::SimplifiedToHongKong => TextConversionMode::SimplifiedToHongKong,
            CTextConversionMode::HongKongToSimplified => TextConversionMode::HongKongToSimplified,
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
    /// 当启用时，smtc-suite 会以 100ms 的频率主动发送 TrackChanged 事件来模拟平滑进度。
    SetHighFrequencyProgressUpdates(bool),
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
    /// 响应 `RequestUpdate` 命令时发送，同样包含一个完整的状态快照。
    TrackChangedForced(NowPlayingInfo),
    /// 可用的媒体会话列表已更新。
    SessionsChanged(Vec<SmtcSessionInfo>),
    /// 接收到一个音频数据包（如果音频捕获已启动）。
    AudioData(Vec<u8>),
    /// 报告一个非致命的运行时错误。
    Error(String),
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

/// 与媒体库后台服务交互的控制器。
///
/// 这是库的公共入口点。它包含一个命令发送端和一个更新接收端，
/// 用于与在后台运行的 `MediaWorker` 进行通信。
pub struct MediaController {
    /// 用于向后台服务发送 `MediaCommand` 的通道发送端。
    pub command_tx: Sender<MediaCommand>,
    /// 用于从后台服务接收 `MediaUpdate` 的通道接收端。
    pub update_rx: Receiver<MediaUpdate>,
}

impl MediaController {
    /// 发送一个 `Shutdown` 命令来优雅地终止媒体服务的后台线程。
    ///
    /// # 返回
    /// - `Ok(())`: 如果命令成功发送。
    /// - `Err(SendError)`: 如果后台线程已经关闭，无法接收命令。
    pub fn shutdown(&self) -> Result<(), SendError<MediaCommand>> {
        self.command_tx.send(MediaCommand::Shutdown)
    }
}
