//! Typed error surface for the whole I/O layer.

/// Errors produced by Micro-Vocal Lab's I/O layer.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Malformed or unusable in-memory audio (zero channels, partial
    /// frames, zero sample rate).
    #[error("invalid audio: {0}")]
    InvalidAudio(String),

    /// The file is not a format we can read.
    #[error("unsupported format: {0}")]
    UnsupportedFormat(String),

    /// WAV read/write failure (hound), with context.
    #[error("WAV error: {0}")]
    Wav(String),

    /// MP3 decode failure (symphonia), with context.
    #[error("MP3 decode error: {0}")]
    Mp3Decode(String),

    /// MP3 encode failure (LAME), with context.
    #[error("MP3 encode error: {0}")]
    Mp3Encode(String),

    /// Resampler failure (rubato), with context.
    #[error("resampling error: {0}")]
    Resample(String),

    /// Audio device / stream failure (cpal), with context.
    #[error("audio device error: {0}")]
    Device(String),

    /// The operation cannot run in the current state (e.g. stopping a
    /// recorder that already stopped, exporting while a render is in
    /// flight).
    #[error("invalid state: {0}")]
    InvalidState(String),

    /// Wrapped I/O error with file context.
    #[error("{context}: {source}")]
    Io {
        /// What we were doing.
        context: String,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
}

impl Error {
    /// Build an [`Error::Io`] with context — used by all file-touching
    /// call sites so error messages always name the operation.
    pub fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }
}

/// Result alias used across the crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;
