//! Shared rate + channel adaptation (used by the file player, the
//! live-preview player, and exporters).
//!
//! v1.0.0 had two *private* copies of a mono↔stereo-only adapter; a
//! device with any other channel count (5.1/7.1 HDMI, aggregate devices)
//! failed to play (BUG 3). One generalized, tested implementation now
//! serves every call site.

use crate::{InterleavedAudio, Result};

/// Adapt `audio` to `rate` / `channels`: resample first (no-op on match),
/// then up/down-mix.
///
/// Channel rules:
/// * N → 1: average all channels (mono downmix).
/// * 1 → N: duplicate mono into every output channel.
/// * N → M (N,M ≥ 2): the first `min(N,M)` channels pass through; when
///   M > N the last source channel repeats into the remaining outputs;
///   when N > M surplus source channels fold into the last output so
///   nothing is silently dropped.
///
/// # Errors
/// [`crate::Error::Resample`] from rate conversion;
/// [`crate::Error::InvalidAudio`] on degenerate shapes.
pub fn adapt(audio: &InterleavedAudio, rate: u32, channels: u16) -> Result<InterleavedAudio> {
    let rate_matched = crate::resample::resample(audio, rate)?;
    if rate_matched.channels == channels {
        return Ok(rate_matched);
    }
    let frames = rate_matched.frames();
    let src_ch = rate_matched.channels as usize;
    let dst_ch = channels as usize;
    let mut mixed = Vec::with_capacity(frames * dst_ch);
    for f in 0..frames {
        let frame = &rate_matched.data[f * src_ch..(f + 1) * src_ch];
        if src_ch == 1 {
            for _ in 0..dst_ch {
                mixed.push(frame[0]);
            }
        } else if dst_ch == 1 {
            mixed.push(frame.iter().sum::<f32>() / src_ch as f32);
        } else {
            let pass = src_ch.min(dst_ch);
            for c in 0..dst_ch {
                if c < pass {
                    mixed.push(frame[c]);
                } else {
                    mixed.push(frame[src_ch - 1]);
                }
            }
            if src_ch > dst_ch {
                let last = mixed.len() - 1;
                for &extra in frame.iter().take(src_ch).skip(dst_ch) {
                    mixed[last] += extra / src_ch as f32;
                }
            }
        }
    }
    InterleavedAudio::new(mixed, rate, channels)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mono_to_stereo_duplicates() {
        let src = InterleavedAudio::new(vec![0.25, -0.75], 48_000, 1).unwrap();
        let out = adapt(&src, 48_000, 2).unwrap();
        assert_eq!(out.data, vec![0.25, 0.25, -0.75, -0.75]);
    }

    #[test]
    fn stereo_to_mono_averages() {
        let src = InterleavedAudio::new(vec![0.5, -0.5, 1.0, 0.0], 48_000, 2).unwrap();
        let out = adapt(&src, 48_000, 1).unwrap();
        assert_eq!(out.data, vec![0.0, 0.5]);
    }

    #[test]
    fn mono_to_six_channel_fills_all() {
        let src = InterleavedAudio::new(vec![0.5, -0.5], 48_000, 1).unwrap();
        let out = adapt(&src, 48_000, 6).unwrap();
        assert_eq!(out.channels, 6);
        assert_eq!(out.data.len(), 12);
        assert!(out.data.iter().take(6).all(|&s| (s - 0.5).abs() < 1e-6));
    }

    #[test]
    fn six_to_two_folds_surplus_into_last() {
        // 6ch frame: [1,0,0,0,0,0] — surplus channels are silent, so the
        // fold must not change the front pair.
        let src = InterleavedAudio::new(vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0], 48_000, 6).unwrap();
        let out = adapt(&src, 48_000, 2).unwrap();
        assert_eq!(out.channels, 2);
        assert!((out.data[0] - 1.0).abs() < 1e-6, "front left changed");
        assert!(out.data[1].abs() < 1e-6, "front right changed");
    }

    #[test]
    fn same_shape_is_clone_path() {
        let src = InterleavedAudio::new(vec![0.1, 0.2, 0.3, 0.4], 48_000, 2).unwrap();
        let out = adapt(&src, 48_000, 2).unwrap();
        assert_eq!(out, src);
    }
}
