//! # mvl-io — Micro-Vocal Lab audio I/O
//!
//! Recording (cpal), WAV/MP3 import & export (hound / symphonia / LAME),
//! high-quality resampling (rubato) and playback with transport control.
//!
//! All public entry points operate on [`InterleavedAudio`] — interleaved
//! `f32` samples — and never panic on user-supplied data: every failure is
//! a typed [`Error`].
//!
//! Module map (populated phase 2): `wav` — WAV import/export via hound,
//! `mp3` — MP3 import (symphonia) and export (LAME), `resample` — rubato
//! rate conversion, `recorder` — 192 kHz/32-bit capture, `player` +
//! `transport` — playback with play/pause/stop/seek.

mod error;
pub mod mp3;
pub mod resample;
pub mod wav;

pub use error::{Error, Result};
pub use mp3::{Mp3Bitrate, Mp3Settings, VbrQuality};
pub use wav::WavDepth;

/// Interleaved `f32` audio with its sample rate and channel count.
///
/// This is the currency of the whole application: the recorder produces
/// it, imports decode into it, the DSP engine consumes and produces it,
/// exporters serialize it.
#[derive(Debug, Clone, PartialEq)]
pub struct InterleavedAudio {
    /// Interleaved sample data (`data[frame * channels + channel]`).
    pub data: Vec<f32>,
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Number of interleaved channels (1 = mono, 2 = stereo).
    pub channels: u16,
}

impl InterleavedAudio {
    /// Construct from parts, validating shape invariants.
    ///
    /// # Errors
    /// [`Error::InvalidAudio`] if the channel count is zero or the buffer
    /// is not a whole number of frames.
    pub fn new(data: Vec<f32>, sample_rate: u32, channels: u16) -> Result<Self, Error> {
        if channels == 0 {
            return Err(Error::InvalidAudio(
                "channel count must be at least 1".into(),
            ));
        }
        if data.len() % channels as usize != 0 {
            return Err(Error::InvalidAudio(
                "sample buffer is not a whole number of interleaved frames".into(),
            ));
        }
        if sample_rate == 0 {
            return Err(Error::InvalidAudio(
                "sample rate must be at least 1 Hz".into(),
            ));
        }
        Ok(Self {
            data,
            sample_rate,
            channels,
        })
    }

    /// Number of frames (one frame = one sample per channel).
    pub fn frames(&self) -> usize {
        self.data.len() / self.channels as usize
    }

    /// Duration in seconds.
    pub fn duration_seconds(&self) -> f64 {
        self.frames() as f64 / self.sample_rate as f64
    }
}

/// Import an audio file, auto-detecting the format from extension and
/// content. WAV via hound (with a symphonia fallback for exotic RIFF
/// dialects), MP3 via symphonia.
///
/// # Errors
/// [`Error::UnsupportedFormat`] for anything that is neither RIFF/WAVE nor
/// MPEG audio, plus decoder errors from the underlying libraries.
pub fn import(path: &std::path::Path) -> Result<InterleavedAudio> {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "wav" | "wave" => wav::import(path)
            .or_else(|e| mp3::import_any(path).map(|(audio, _)| audio).map_err(|_| e)),
        "mp3" => mp3::import(path),
        _ => {
            // Unknown extension: sniff the content.
            if mp3::looks_like_mp3(path)? {
                mp3::import(path)
            } else if wav::looks_like_wav(path)? {
                wav::import(path)
            } else {
                Err(Error::UnsupportedFormat(format!(
                    "'{}' is not a supported audio format (WAV or MP3)",
                    path.display()
                )))
            }
        }
    }
}
