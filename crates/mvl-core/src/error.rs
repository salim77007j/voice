//! Engine error type.
//!
//! The DSP chain itself never fails on finite input — parameters are
//! pre-clamped by [`crate::VocalParams::sanitized`] and every internal
//! buffer length is fixed at construction. Errors therefore only cover
//! degenerate caller input and (unexpected) construction failures.

/// Errors from the vocal DSP engine.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// The session sample rate was zero.
    #[error("sample rate must be at least 1 Hz, got {0}")]
    ZeroRate(u32),
    /// Input contained NaN/Inf samples.
    #[error("input contains non-finite samples (NaN/Inf) at index {0}")]
    NonFiniteInput(usize),
    /// An internal invariant was violated. This is a bug, not a user
    /// error — the message identifies where.
    #[error("engine internal error: {0}")]
    Internal(String),
}
