//! Audio subsystem error types.

use thiserror::Error;

/// Errors that can occur in the audio subsystem.
#[derive(Error, Debug)]
pub enum AudioError {
    /// Failed to initialize the audio engine or start the stream.
    #[error("Audio initialization failed: {0}")]
    Init(String),

    /// Failed to decode an audio file.
    #[error("Audio decode failed: {0}")]
    Decode(String),

    /// The requested audio device was not found.
    #[error("Audio device not found: {0}")]
    DeviceNotFound(String),

    /// The audio stream encountered an error and was stopped.
    #[error("Audio stream error: {0}")]
    Stream(String),
}
