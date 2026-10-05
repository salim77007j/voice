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
//! `transport` — playback with play/pause/stop/seek, `playhead` —
//! device-clock anchored audible-position tracking (phase 8.2 sync fix).

pub mod channels;
pub mod consent;
pub mod devices;
mod error;
pub mod mp3;
pub mod player;
pub mod playhead;
pub mod recorder;
pub mod resample;
pub mod transport;
pub mod wav;

pub use devices::{DeviceInfo, DeviceInventory};
pub use error::{Error, Result};
pub use mp3::{Mp3Bitrate, Mp3Settings, VbrQuality};
pub use player::{Player, PlayerCommand};
pub use playhead::{audible_position_seconds, host_output_latency, PlayheadAnchor};
pub use recorder::{RecordRequest, RecordingStats, SampleFmt};
pub use transport::{Transport, TransportState};
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

/// Hard ceiling on imported (decoded) audio: 512 Mi frames-worth of f32
/// sample data ≈ 2 GiB. Files that would decode larger are refused with
/// a clear message instead of dragging the machine into an OOM kill —
/// the app must never crash (BUG 4). For scale: 3 h of stereo 44.1 kHz
/// is ~0.95 G samples and still imports; pathological multi-hour
/// high-rate material belongs in the disk-streamed *recording* path.
pub const MAX_IMPORT_SAMPLES: u64 = 512 * 1024 * 1024;

/// Reject absurdly large files **before** decoding, using the on-disk
/// size as a proxy (BUG 2 hardening: a truncated read of a 20 GiB WAV
/// header must not start a decode that can only end in OOM).
///
/// Bounds used (decoded f32 bytes vs file bytes, worst case per format):
/// WAV: `×4` (8-bit PCM decodes to 32-bit floats), MP3: `×8`
/// (32 kbps mono 8 kHz expands ~8×).
fn guard_import_size(path: &std::path::Path) -> Result<()> {
    const MAX_BYTES: u64 = MAX_IMPORT_SAMPLES * 4;
    let meta = std::fs::metadata(path)
        .map_err(|e| Error::io(format!("read metadata of '{}'", path.display()), e))?;
    if !meta.is_file() {
        return Err(Error::UnsupportedFormat(format!(
            "'{}' is not a regular file",
            path.display()
        )));
    }
    let size = meta.len();
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    // Worst-case decoded-f32 growth per on-disk byte:
    // WAV ×4 (8-bit PCM → 32-bit float), MP3 ×8 (32 kbps mono 8 kHz),
    // unknown extensions sniffed as either — keep the pessimistic 8×.
    let expansion: u64 = match ext.as_str() {
        "wav" | "wave" => 4,
        _ => 8,
    };
    let bound = size.saturating_mul(expansion);
    if bound > MAX_BYTES {
        return Err(Error::UnsupportedFormat(format!(
            "'{}' is too large to import ({:.2} GiB on disk; decoded audio would exceed the {:.0} GiB safety limit). \
             Import a shorter file, or record it directly in Micro-Vocal Lab (recordings stream to disk \
             and have no length limit).",
            path.display(),
            size as f64 / (1024.0 * 1024.0 * 1024.0),
            MAX_BYTES as f64 / (1024.0 * 1024.0 * 1024.0),
        )));
    }
    Ok(())
}

/// Import an audio file, auto-detecting the format from extension and
/// content. WAV via hound (with a symphonia fallback for exotic RIFF
/// dialects), MP3 via symphonia.
///
/// Never panics on user files: empty files, zero-frame files, corrupt
/// headers and oversize files all return typed errors (BUG 2 regression
/// surface).
///
/// # Errors
/// [`Error::UnsupportedFormat`] for anything that is neither RIFF/WAVE nor
/// MPEG audio, for empty/zero-frame files, and for files too large to
/// import; plus decoder errors from the underlying libraries.
pub fn import(path: &std::path::Path) -> Result<InterleavedAudio> {
    guard_import_size(path)?;
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let audio = match ext.as_str() {
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
                return Err(Error::UnsupportedFormat(format!(
                    "'{}' is not a supported audio format (WAV or MP3)",
                    path.display()
                )));
            }
        }
    }?;
    if audio.frames() == 0 {
        return Err(Error::UnsupportedFormat(format!(
            "'{}' contains no audio frames (empty or header-only file)",
            path.display()
        )));
    }
    if audio.data.len() as u64 > MAX_IMPORT_SAMPLES {
        return Err(Error::UnsupportedFormat(format!(
            "'{}' decodes to {:.2} GiB of audio, above the {:.0} GiB import limit",
            path.display(),
            audio.data.len() as f64 * 4.0 / (1024.0 * 1024.0 * 1024.0),
            MAX_IMPORT_SAMPLES as f64 * 4.0 / (1024.0 * 1024.0 * 1024.0),
        )));
    }
    Ok(audio)
}
