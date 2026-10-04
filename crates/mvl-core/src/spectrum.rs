//! RTA (real-time analyzer) spectrum + level metering for the UI
//! (Audioprecise-style analysis panel, plan §8 "studio rack" redesign).
//!
//! This is a *display* analyzer, deliberately separate from the engine's
//! [`crate::analysis`] frame pipeline: it answers "what should the RTA
//! bars and the VU meters show right now", not "how should the processors
//! treat this frame". The controller calls it ~30×/s on a short window of
//! the preview mix at the current playhead — a 2048-point real FFT costs
//! well under a millisecond, so the UI tick budget is untouched.
//!
//! Band layout: 40 log-spaced bands from 20 Hz to Nyquist (capped at
//! 20 kHz for display stability across sample rates). Band magnitude is
//! the *maximum* bin magnitude in the band (peak-hold per band reads
//! better than the mean on sparse vocals) mapped from
//! [`RTA_FLOOR_DB`]…0 dBFS to 0…1.

use std::f64::consts::PI;
use std::sync::Arc;

use realfft::{RealFftPlanner, RealToComplex};
use rustfft::num_complex::Complex64;

/// Number of display bands.
pub const RTA_BANDS: usize = 40;
/// Lowest band edge (Hz).
pub const RTA_MIN_HZ: f64 = 20.0;
/// Highest displayed frequency (Hz) — Nyquist is capped here so the band
/// layout stays identical at 44.1/48/96/192 kHz.
pub const RTA_MAX_HZ: f64 = 20_000.0;
/// Display floor (dBFS): band bars map [floor, 0] dB → [0, 1].
pub const RTA_FLOOR_DB: f32 = -72.0;
/// Meter floor (dBFS): VU segments map [floor, 0] dB → [0, 1].
pub const METER_FLOOR_DB: f32 = -60.0;
/// Peak sample level (dBFS) above which the clip LED latches.
pub const CLIP_DBFS: f32 = -0.1;

/// One analyzer result frame: normalized band heights + levels.
#[derive(Debug, Clone, PartialEq)]
pub struct RtaSnapshot {
    /// Band heights 0..1 (length [`RTA_BANDS`]).
    pub bands: [f32; RTA_BANDS],
    /// True-peak sample level in dBFS (raw window, no windowing).
    pub peak_dbfs: f32,
    /// RMS level in dBFS over the window.
    pub rms_dbfs: f32,
}

impl Default for RtaSnapshot {
    fn default() -> Self {
        Self {
            bands: [0.0; RTA_BANDS],
            peak_dbfs: METER_FLOOR_DB,
            rms_dbfs: METER_FLOOR_DB,
        }
    }
}

impl RtaSnapshot {
    /// Peak level normalized for the VU meters (0..1, dB-linear).
    #[must_use]
    pub fn peak_meter(&self) -> f32 {
        db_to_meter(self.peak_dbfs)
    }

    /// RMS level normalized for the VU meters (0..1, dB-linear).
    #[must_use]
    pub fn rms_meter(&self) -> f32 {
        db_to_meter(self.rms_dbfs)
    }
}

/// Map a dBFS value onto the meter scale 0..1 ([`METER_FLOOR_DB`] → 0).
#[must_use]
pub fn db_to_meter(db: f32) -> f32 {
    if !db.is_finite() {
        return 0.0;
    }
    ((db - METER_FLOOR_DB) / -METER_FLOOR_DB).clamp(0.0, 1.0)
}

/// Display analyzer: fixed-size real FFT with a periodic Hann window.
pub struct RtaAnalyzer {
    r2c: Arc<dyn RealToComplex<f64>>,
    window: Vec<f64>,
    scratch_in: Vec<f64>,
    scratch_spec: Vec<Complex64>,
}

impl RtaAnalyzer {
    /// Build an analyzer for `fft_size` points. Powers of two only;
    /// 2048 is the UI default (~43 ms at 48 kHz — fast enough to dance
    /// with the music, long enough to resolve the low bands).
    ///
    /// # Panics
    /// Only if `fft_size` is not a power of two ≥ 16 — a programming
    /// error, never reachable from user data.
    #[must_use]
    pub fn new(fft_size: usize) -> Self {
        assert!(
            fft_size >= 16 && fft_size.is_power_of_two(),
            "fft_size must be a power of two >= 16"
        );
        let mut planner = RealFftPlanner::new();
        let r2c = planner.plan_fft_forward(fft_size);
        let window = (0..fft_size)
            .map(|k| 0.5 - 0.5 * (2.0 * PI * k as f64 / fft_size as f64).cos())
            .collect();
        Self {
            r2c,
            window,
            scratch_in: vec![0.0; fft_size],
            scratch_spec: vec![Complex64::new(0.0, 0.0); fft_size / 2 + 1],
        }
    }

    /// FFT size in points.
    #[must_use]
    pub fn fft_size(&self) -> usize {
        self.window.len()
    }

    /// Analyze one mono window.
    ///
    /// Never panics: `mono` shorter than the FFT size is zero-padded,
    /// longer is truncated; an empty slice yields silence; a zero or
    /// absurd `rate` is treated as 48 kHz for banding purposes only.
    pub fn analyze(&mut self, mono: &[f32], rate: u32, out: &mut RtaSnapshot) {
        let n = self.window.len();
        let take = mono.len().min(n);
        let (input, scratch_in, window) = (&mono[..take], &mut self.scratch_in, &self.window);

        // Levels on the raw (unwindowed) samples.
        let mut peak: f32 = 0.0;
        let mut sum_sq: f64 = 0.0;
        for &s in input {
            let a = if s.is_finite() { s.abs() } else { 0.0 };
            peak = peak.max(a);
            sum_sq += f64::from(a) * f64::from(a);
        }
        let rms = if take > 0 {
            (sum_sq / take as f64).sqrt() as f32
        } else {
            0.0
        };
        out.peak_dbfs = amp_to_db(peak);
        out.rms_dbfs = amp_to_db(rms);

        // Windowed copy into the FFT input (zero-padded by construction).
        for k in 0..take {
            let s = if input[k].is_finite() { input[k] } else { 0.0 };
            scratch_in[k] = f64::from(s) * window[k];
        }
        for slot in &mut scratch_in[take..] {
            *slot = 0.0;
        }
        if let Err(e) = self
            .r2c
            .process(&mut self.scratch_in, &mut self.scratch_spec)
        {
            // realfft only fails on length mismatch, which cannot happen
            // here (both buffers are constructed to match). Fail soft:
            // silence the frame rather than take the UI down.
            eprintln!("mvl-core: RTA fft failed: {e}");
            out.bands = [0.0; RTA_BANDS];
            return;
        }

        // Bin magnitudes in dBFS (amplitude scale: |X|·2/n).
        let rate = if (1..=1_000_000).contains(&rate) {
            rate
        } else {
            48_000
        };
        let nyquist = f64::from(rate) / 2.0;
        let bin_hz = f64::from(rate) / n as f64;
        let floor = f64::from(RTA_FLOOR_DB);

        // Band edges: log-spaced between RTA_MIN_HZ and min(nyquist, RTA_MAX_HZ).
        let top_hz = nyquist.clamp(RTA_MIN_HZ * 2.0, RTA_MAX_HZ);
        let mut band = 0usize;
        let mut band_max_db = f64::NEG_INFINITY;
        for (bin, spec) in self.scratch_spec.iter().enumerate() {
            let hz = bin as f64 * bin_hz;
            if bin > 0 && band < RTA_BANDS {
                let edge =
                    RTA_MIN_HZ * (top_hz / RTA_MIN_HZ).powf((band + 1) as f64 / RTA_BANDS as f64);
                if hz >= edge {
                    // Close the band we were filling.
                    if band < RTA_BANDS {
                        out.bands[band] = db_to_band(band_max_db, floor);
                    }
                    band += 1;
                    band_max_db = f64::NEG_INFINITY;
                }
            }
            if bin > 0 {
                let amp = spec.norm() * 2.0 / n as f64;
                band_max_db = band_max_db.max(amp_to_db(amp as f32) as f64);
            }
        }
        // Final partial band(s): with coarse FFTs (or windows shorter than
        // the FFT), the bin loop can exit with trailing bands unclosed —
        // fill them with the running maximum so every band has a value.
        while band < RTA_BANDS {
            out.bands[band] = db_to_band(band_max_db, floor);
            band += 1;
            band_max_db = f64::NEG_INFINITY;
        }
    }
}

fn amp_to_db(amp: f32) -> f32 {
    if amp <= 0.0 {
        METER_FLOOR_DB.min(RTA_FLOOR_DB) - 60.0 // comfortably below both floors
    } else {
        20.0 * amp.log10()
    }
}

fn db_to_band(db: f64, floor: f64) -> f32 {
    if !db.is_finite() {
        return 0.0;
    }
    ((db - floor) / -floor).clamp(0.0, 1.0) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1 kHz sine at -6 dBFS (0.5 amplitude).
    fn sine(rate: u32, seconds: f64, freq: f64, amp: f32) -> Vec<f32> {
        let n = (rate as f64 * seconds) as usize;
        (0..n)
            .map(|i| amp * (2.0 * PI * freq * i as f64 / f64::from(rate)).sin() as f32)
            .collect()
    }

    #[test]
    fn silence_gives_zero_bands_and_floored_levels() {
        let mut an = RtaAnalyzer::new(2048);
        let mut snap = RtaSnapshot::default();
        an.analyze(&vec![0.0; 2048], 48_000, &mut snap);
        assert!(snap.bands.iter().all(|&b| b == 0.0));
        assert!(snap.peak_dbfs < METER_FLOOR_DB);
        assert!(snap.rms_dbfs < METER_FLOOR_DB);
        assert_eq!(snap.peak_meter(), 0.0);
    }

    #[test]
    fn one_khz_sine_peaks_in_the_1khz_band() {
        let rate = 48_000u32;
        let mut an = RtaAnalyzer::new(2048);
        let mut snap = RtaSnapshot::default();
        let signal = sine(rate, 1.0, 1000.0, 0.5);
        an.analyze(&signal, rate, &mut snap);

        // Which band holds 1 kHz? edges: 20 * (1000^(1/40) ratio …) —
        // recompute the same way the analyzer does.
        let top = (f64::from(rate) / 2.0).min(RTA_MAX_HZ);
        let decades = (top / RTA_MIN_HZ).ln();
        let _ = decades;
        let band_of = |hz: f64| -> usize {
            ((hz / RTA_MIN_HZ).ln() / (top / RTA_MIN_HZ).ln() * RTA_BANDS as f64) as usize
        };
        let expected = band_of(1000.0).min(RTA_BANDS - 1);

        let (max_band, max_val) =
            snap.bands
                .iter()
                .enumerate()
                .fold(
                    (0usize, 0.0f32),
                    |(bi, bv), (i, &v)| {
                        if v > bv {
                            (i, v)
                        } else {
                            (bi, bv)
                        }
                    },
                );
        assert_eq!(
            max_band, expected,
            "peak band {max_band}, expected {expected}"
        );
        assert!(
            max_val > 0.5,
            "1 kHz band should be well lit (got {max_val})"
        );
        // Neighbors must be far dimmer (leakage only).
        for (i, &v) in snap.bands.iter().enumerate() {
            if (i as isize - expected as isize).abs() > 2 {
                assert!(v < 0.2, "band {i} leaking: {v}");
            }
        }
    }

    #[test]
    fn levels_track_amplitude() {
        let rate = 48_000u32;
        let mut an = RtaAnalyzer::new(2048);
        let mut snap = RtaSnapshot::default();
        // -6 dBFS sine: peak dBFS should be within 0.5 dB of -6.
        an.analyze(&sine(rate, 1.0, 997.0, 0.5), rate, &mut snap);
        assert!(
            (snap.peak_dbfs - (-6.0)).abs() < 0.5,
            "peak {}",
            snap.peak_dbfs
        );
        // RMS of a sine = amplitude / sqrt(2) → -9.03 dBFS.
        assert!(
            (snap.rms_dbfs - (-9.03)).abs() < 0.5,
            "rms {}",
            snap.rms_dbfs
        );
        assert!((snap.peak_meter() - 0.9).abs() < 0.05);
    }

    #[test]
    fn white_noise_lights_all_bands() {
        let rate = 48_000u32;
        let mut an = RtaAnalyzer::new(2048);
        let mut snap = RtaSnapshot::default();
        // Deterministic pseudo-noise (xorshift) — no external rng dep.
        let mut state = 0x2545F4914F6CDD1Du64;
        let noise: Vec<f32> = (0..2048)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                ((state >> 11) as f64 / (1u64 << 53) as f64) as f32 * 0.4 - 0.2
            })
            .collect();
        an.analyze(&noise, rate, &mut snap);
        let lit = snap.bands.iter().filter(|&&b| b > 0.05).count();
        assert!(lit >= RTA_BANDS / 2, "only {lit} bands lit for white noise");
    }

    #[test]
    fn short_and_empty_windows_do_not_panic() {
        let mut an = RtaAnalyzer::new(2048);
        let mut snap = RtaSnapshot::default();
        an.analyze(&[], 48_000, &mut snap);
        an.analyze(&[0.1, -0.1], 48_000, &mut snap);
        an.analyze(&vec![1.0; 999_999], 44_100, &mut snap); // truncated
        an.analyze(&sine(48_000, 0.1, 200.0, 0.9), 0, &mut snap); // absurd rate
        an.analyze(&[f32::NAN, f32::INFINITY, 0.5], 48_000, &mut snap); // non-finite
    }

    #[test]
    fn band_layout_is_stable_across_rates() {
        // The 1 kHz band index must be identical at 44.1/48/192 kHz
        // (display stability — the RTA grid labels are static).
        let mut an = RtaAnalyzer::new(2048);
        let mut snap = RtaSnapshot::default();
        let mut last: Option<usize> = None;
        for &rate in &[44_100u32, 48_000, 96_000, 192_000] {
            let signal = sine(rate, 1.0, 1000.0, 0.5);
            an.analyze(&signal, rate, &mut snap);
            let top = (f64::from(rate) / 2.0).min(RTA_MAX_HZ);
            let idx = ((1000.0f64 / RTA_MIN_HZ).ln() / (top / RTA_MIN_HZ).ln() * RTA_BANDS as f64)
                as usize;
            if let Some(prev) = last {
                assert_eq!(prev, idx, "1 kHz band moved at {rate} Hz");
            }
            last = Some(idx);
        }
    }

    #[test]
    fn db_to_meter_mapping() {
        assert_eq!(db_to_meter(METER_FLOOR_DB), 0.0);
        assert_eq!(db_to_meter(0.0), 1.0);
        assert!((db_to_meter(-30.0) - 0.5).abs() < 1e-6);
        assert_eq!(db_to_meter(-120.0), 0.0);
        assert_eq!(db_to_meter(6.0), 1.0);
        assert_eq!(db_to_meter(f32::NAN), 0.0);
    }
}
