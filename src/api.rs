use std::{
    sync::mpsc::{self, Receiver, Sender},
    time::Instant,
};

/// 包含当前正在播放曲目的详细信息。
///
/// 这个结构体聚合了从系统媒体传输控件 (SMTC) 获取的所有相关信息，
/// 例如歌曲标题、艺术家、时长、播放状态和封面艺术。
#[derive(Debug, Clone, Default, PartialEq)]
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
    /// 这是一个估算值，结合了 SMTC 上次报告的位置和自那时起经过的时间。
    pub position_ms: Option<u64>,
    /// 是否正在播放。
    pub is_playing: Option<bool>,
    /// 封面图片的原始字节数据。
    pub cover_data: Option<Vec<u8>>,
    /// 封面数据的哈希值，用于快速比较封面是否已更改。
    pub cover_data_hash: Option<u64>,
    /// SMTC 上次报告播放位置的时间点。
    pub position_report_time: Option<Instant>,
}

/// 表示一个可用的系统媒体传输控件 (SMTC) 会话。
///
/// 每个支持媒体控制的应用（如 Spotify, Windows Media Player）都会创建一个会话。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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
    /// 请求关闭整个媒体服务后台线程。
    Shutdown,
}

/// 从媒体库后台服务接收的事件和状态更新。
///
/// 使用者通过 `MediaController` 的 `update_rx` 通道接收这些更新。
#[derive(Debug, Clone)]
pub enum MediaUpdate {
    /// 当前播放的曲目信息已发生变化。
    TrackChanged(NowPlayingInfo),
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
    pub fn shutdown(&self) -> std::result::Result<(), mpsc::SendError<MediaCommand>> {
        self.command_tx.send(MediaCommand::Shutdown)
    }
}
