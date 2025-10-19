use thiserror::Error;
use tokio::sync::mpsc::error::SendError;

use crate::api::MediaCommand;
use crate::audio_capture::AudioCaptureError;

#[derive(Debug, Error)]
pub enum SmtcError {
    #[error("Unable to start worker thread: {0}")]
    WorkerThread(String),

    #[error("Windows API call failed: {0}")]
    Windows(#[from] windows::core::Error),

    #[error("Audio resampling process failed: {0}")]
    Resample(#[from] rubato::ResampleError),

    #[error("Audio resampler creation failed: {0}")]
    ResamplerConstruction(#[from] rubato::ResamplerConstructionError),

    #[error("Failed to send external command to worker thread")]
    CommandSendError(#[from] SendError<MediaCommand>),

    #[error("Audio capture failed: {0}")]
    AudioCapture(#[from] AudioCaptureError),

    #[error("Volume control failed: {0}")]
    VolumeControl(String),

    #[error("Tokio runtime creation failed: {0}")]
    TokioRuntime(#[from] std::io::Error),

    #[error("The media service is already running and cannot be started again.")]
    AlreadyRunning,

    #[error("The lock has been poisoned: {0}")]
    MutexPoisoned(String),
}

impl<T> From<std::sync::PoisonError<T>> for SmtcError {
    fn from(err: std::sync::PoisonError<T>) -> Self {
        Self::MutexPoisoned(err.to_string())
    }
}

pub type Result<T> = std::result::Result<T, SmtcError>;
