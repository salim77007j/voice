//! High-quality sample-rate conversion via [rubato].
//!
//! Uses the synchronous FFT resampler in `f64` (internally) with the
//! one-shot `process_all` path, which resets state and trims the
//! resampler's group delay — the output is time-aligned with the input and
//! exactly `round(frames × out_rate / in_rate)` frames long (±1 frame).
//!
//! This module is the workhorse behind the 192 kHz → 48 kHz preview copy
//! and MP3-export rate conversion (LAME tops out at 48 kHz), per the
//! architecture plan §5.3 and §9.2.

use std::path::Path;

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Resampler};

use crate::{Error, InterleavedAudio, Result};

/// Frames per internal chunk for the FFT resampler. 8192 keeps the
/// transform efficient while bounding latency of the offline path.
const CHUNK_FRAMES: usize = 8192;

/// Resample interleaved audio to `target_rate`.
///
/// If the audio is already at the target rate the input is returned
/// unchanged (a clone — the original is never mutated). Empty audio is
/// passed through with the new rate.
///
/// # Errors
/// * [`Error::InvalidAudio`] — degenerate input
/// * [`Error::Resample`] — resampler construction or processing failure
pub fn resample(audio: &InterleavedAudio, target_rate: u32) -> Result<InterleavedAudio> {
    if target_rate == 0 {
        return Err(Error::InvalidAudio(
            "target sample rate must be at least 1 Hz".into(),
        ));
    }
    if audio.sample_rate == target_rate {
        return Ok(audio.clone());
    }
    let channels = audio.channels as usize;
    let frames = audio.frames();
    if frames == 0 {
        return InterleavedAudio::new(Vec::new(), target_rate, audio.channels);
    }

    // Non-finite input would poison the FFT; fail loudly (imports are
    // already validated, this guards programmatic callers).
    if audio.data.iter().any(|s| !s.is_finite()) {
        return Err(Error::InvalidAudio(
            "cannot resample non-finite (NaN/Inf) samples".into(),
        ));
    }

    // rubato's FFT resampler computes in f64 — convert up for the
    // transform and back down after (quality over speed on this path).
    let input_f64: Vec<f64> = audio.data.iter().map(|&s| f64::from(s)).collect();

    let mut resampler = Fxt::new(
        audio.sample_rate as usize,
        target_rate as usize,
        CHUNK_FRAMES,
        channels,
        FixedSync::Both,
    )
    .map_err(|e| {
        Error::Resample(format!(
            "build resampler {}->{} Hz: {e}",
            audio.sample_rate, target_rate
        ))
    })?;

    let in_adapter = InterleavedSlice::new(&input_f64, channels, frames)
        .map_err(|e| Error::Resample(format!("prepare input buffer: {e}")))?;

    let out_owned = resampler
        .process_all(&in_adapter, frames, None)
        .map_err(|e| {
            Error::Resample(format!(
                "resample {}->{} Hz: {e}",
                audio.sample_rate, target_rate
            ))
        })?;

    let data = out_owned
        .take_data()
        .into_iter()
        .map(|s| s as f32)
        .collect::<Vec<f32>>();

    InterleavedAudio::new(data, target_rate, audio.channels)
}

/// Alias so the main path reads like prose.
type Fxt = Fft<f64>;

/// Resample a file on disk from one rate to another, writing a float WAV.
/// Convenience helper used by the CLI/app layer for the 48 kHz preview
/// copy of a recording (plan §9.2).
///
/// # Errors
/// Composes [`crate::wav::import`], [`resample`] and [`crate::wav::export`]
/// errors unchanged.
pub fn resample_file(src: &Path, dst: &Path, target_rate: u32) -> Result<()> {
    let audio = crate::wav::import(src)?;
    let resampled = resample(&audio, target_rate)?;
    crate::wav::export(dst, &resampled, crate::WavDepth::Float32)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 440 Hz sine, amplitude 0.8.
    fn sine(frames: usize, rate: u32, freq: f64) -> Vec<f32> {
        (0..frames)
            .map(|i| {
                (2.0 * std::f64::consts::PI * freq * i as f64 / rate as f64).sin() as f32 * 0.8
            })
            .collect()
    }

    /// Goertzel power at `freq` — a one-bin DFT, enough to verify the
    /// dominant tone survived resampling.
    fn goertzel_power(data: &[f32], rate: u32, freq: f64) -> f64 {
        let k = 2.0 * std::f64::consts::PI * freq / rate as f64;
        let (mut s1, mut s2) = (0.0f64, 0.0f64);
        let coeff = 2.0 * k.cos();
        for &x in data {
            let s0 = f64::from(x) + coeff * s1 - s2;
            s2 = s1;
            s1 = s0;
        }
        s1 * s1 + s2 * s2 - coeff * s1 * s2
    }

    #[test]
    fn identity_rate_returns_clone() {
        let audio = InterleavedAudio::new(vec![0.5, -0.25, 0.75, -0.125], 48_000, 2).unwrap();
        let out = resample(&audio, 48_000).unwrap();
        assert_eq!(out, audio);
    }

    #[test]
    fn duration_is_preserved_within_one_frame() {
        // 1 second of stereo at 192 kHz -> 48 kHz must be 48_000 frames ±1.
        let frames = 192_000;
        let audio = InterleavedAudio::new(sine(frames, 192_000, 440.0), 192_000, 1).unwrap();
        let out = resample(&audio, 48_000).unwrap();
        let expected = 48_000.0f64;
        let got = out.frames() as f64;
        assert!(
            (got - expected).abs() <= 1.5,
            "resampled frame count {got} too far from {expected}"
        );
        assert_eq!(out.sample_rate, 48_000);
    }

    #[test]
    fn tone_survives_downsample_192k_to_48k() {
        let frames = 192_000; // 1 s
        let audio = InterleavedAudio::new(sine(frames, 192_000, 440.0), 192_000, 1).unwrap();
        let out = resample(&audio, 48_000).unwrap();
        // Trim edges (filter ringing) before measuring.
        let core = &out.data[1_000..out.data.len() - 1_000];
        let p_signal = goertzel_power(core, 48_000, 440.0);
        let p_neighbor = goertzel_power(core, 48_000, 700.0);
        assert!(
            p_signal > 100.0 * p_neighbor,
            "440 Hz tone lost in resampling (signal {p_signal:.3} vs neighbor {p_neighbor:.3})"
        );
    }

    #[test]
    fn stereo_channels_stay_separated() {
        // Left = 440 Hz, right = silence; after resampling the right
        // channel must remain (near-)silent — catches channel-index bugs.
        let frames = 96_000;
        let left = sine(frames, 96_000, 440.0);
        let mut interleaved = Vec::with_capacity(frames * 2);
        for l in &left {
            interleaved.push(*l);
            interleaved.push(0.0);
        }
        let audio = InterleavedAudio::new(interleaved, 96_000, 2).unwrap();
        let out = resample(&audio, 48_000).unwrap();
        let right_max = out.data[1..]
            .iter()
            .step_by(2)
            .fold(0.0f32, |m, &x| m.max(x.abs()));
        assert!(right_max < 1e-3, "right channel leaked {right_max}");
        let left_max = out
            .data
            .chunks(2)
            .map(|c| c[0].abs())
            .fold(0.0f32, |m, x| m.max(x));
        assert!(left_max > 0.5, "left channel vanished");
    }

    #[test]
    fn rejects_non_finite_input() {
        let audio = InterleavedAudio::new(vec![0.0, f32::NAN, 0.0], 44_100, 1).unwrap();
        let err = resample(&audio, 48_000).unwrap_err();
        assert!(matches!(err, Error::InvalidAudio(_)), "got {err:?}");
    }

    #[test]
    fn empty_audio_passes_through_with_new_rate() {
        let audio = InterleavedAudio::new(Vec::new(), 192_000, 2).unwrap();
        let out = resample(&audio, 48_000).unwrap();
        assert_eq!(out.frames(), 0);
        assert_eq!(out.sample_rate, 48_000);
        assert_eq!(out.channels, 2);
    }
}
