use crossbeam_channel::SendError;
use thiserror::Error;
/// 定义库的统一错误枚举。
#[derive(Debug, Error)]
pub enum SmtcError {
    /// 无法启动后台工作线程。
    ///
    /// 这通常发生在 `std::thread::Builder::spawn` 失败时。
    #[error("无法启动后台工作线程: {0}")]
    WorkerThread(String),

    /// 底层的 Windows API 或 COM 调用失败。
    ///
    /// 这是最常见的错误类型之一，封装了来自 `windows-rs` crate 的 `windows::core::Error`。
    #[error("Windows API 调用失败: {0}")]
    Windows(#[from] windows::core::Error),

    /// 音频重采样处理过程中发生错误。
    ///
    /// 封装了来自 `rubato` 库的 `ResampleError`。
    #[error("音频重采样处理失败: {0}")]
    Resample(#[from] rubato::ResampleError),

    /// 创建音频重采样器实例时失败。
    ///
    /// 封装了来自 `rubato` 库的 `ResamplerConstructionError`。
    #[error("音频重采样器创建失败: {0}")]
    ResamplerConstruction(#[from] rubato::ResamplerConstructionError),

    /// 向工作线程的命令通道发送命令时失败。
    ///
    /// 这通常意味着后台工作线程已经崩溃或关闭。
    #[error("向工作线程发送命令失败")]
    CommandSendError(#[from] SendError<crate::MediaCommand>),

    /// 音频捕获模块报告的特定逻辑错误。
    #[error("音频捕获失败: {0}")]
    AudioCapture(String),

    /// 音量控制模块报告的特定逻辑错误。
    #[error("音量控制失败: {0}")]
    VolumeControl(String),

    /// 创建 Tokio 异步运行时失败。
    ///
    /// 这是一个严重的初始化错误，会导致后台线程无法启动。
    #[error("Tokio 运行时创建失败: {0}")]
    TokioRuntime(#[from] std::io::Error),
}

/// 本库统一的 `Result` 类型别名。
///
/// 它默认使用 `SmtcError` 作为错误类型，简化了函数签名。
pub type Result<T> = std::result::Result<T, SmtcError>;
