//! Formant shifter — perceived vocal-tract length in millimetres
//! (architecture plan §6.3).
//!
//! Per frame: iterative **true-envelope** estimation (cepstral domain,
//! F0-aware lifter width — converges to the physical envelope even when
//! harmonics are sparse, where LPC misestimates), then a Bark-scale
//! 64-point envelope warp `f → f·g` (`g = L_ref/L`, uniform-tube
//! physics: formant frequencies scale inversely with tract length), and
//! finally resynthesis as a regularised per-bin multiplicative field
//! `Y = X · Ẽ_warp / Ẽ_orig` — regularised so bins near spectral zeros
//! cannot explode (envelope floor, ±18 dB ratio clamp, critical-band
//! smoothing ⇒ no musical noise / birdie artifacts).
//!
//! The stage is magnitude-only: phases and therefore F0 are untouched
//! (engine invariant #4). The engine feeds it `g_user / r_pitch` so the
//! net formant displacement equals the user's mm slider regardless of
//! the pitch setting (the anti-chipmunk contract, plan §6.2 step 3).

use std::sync::Arc;

use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};
use rustfft::num_complex::Complex64;
/// Number of Bark-spaced envelope control points (plan §6.3).
pub const BARK_POINTS: usize = 64;
/// True-envelope iteration count (plan: 4–6).
const ENVELOPE_ITERATIONS: usize = 5;
/// Envelope floor relative to the frame's envelope maximum (dB).
const ENVELOPE_FLOOR_DB: f64 = -80.0;
/// Per-bin ratio clamp (dB) — spectral-zero protection.
const RATIO_CLAMP_DB: f64 = 18.0;
/// Level reduction per Nyquist fold-back (dB).
const FOLD_ATTENUATION_DB: f64 = -6.0;
/// Below this |g − 1| the warp is an exact no-op field.
const IDENTITY_EPSILON: f64 = 1e-6;

/// Zwicker Bark scale.
#[must_use]
pub fn bark(f: f64) -> f64 {
    13.0 * (0.000_76 * f).atan() + 3.5 * ((f / 7500.0).powi(2)).atan()
}

/// Invert the Bark scale by bisection (monotone on `f ≥ 0`).
#[must_use]
pub fn bark_inverse(b: f64, f_hi: f64) -> f64 {
    let mut lo = 0.0f64;
    let mut hi = f_hi.max(1.0);
    for _ in 0..64 {
        let mid = 0.5 * (lo + hi);
        if bark(mid) < b {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    0.5 * (lo + hi)
}

/// Per-frame formant processing state (plans and control grid; the
/// estimation itself is stateless across frames).
pub struct FormantProcessor {
    bins: usize,
    rate: u32,
    nyquist: f64,
    r2c: Arc<dyn RealToComplex<f64>>,
    c2r: Arc<dyn ComplexToReal<f64>>,
    /// Frequencies of the Bark-spaced control points (Hz, ascending) —
    /// kept for the UI's envelope view and diagnostics.
    control_freqs: Vec<f64>,
    /// Per-bin one-pole smoothing of the ratio field across frames.
    /// Essential: any per-frame estimate wobble amplitude-modulates the
    /// harmonics at the frame rate and creates audible AM sidebands at
    /// |f0 − frame_rate| (found by the engine invariant tests).
    prev_ratio_db: Vec<f64>,
    smoothing_init: bool,
    smoothing_coef: f64,
    /// Per-bin Bark smoothing width (in bins, ~0.375 Bark) — the
    /// anti-birdie guarantee: the field can never ripple between
    /// adjacent bins.
    smooth_width_bins: Vec<usize>,
    /// Cepstral transform length (next pow2 ≥ bins).
    cep_len: usize,
}

impl FormantProcessor {
    /// Create for an STFT of `fft_size` points at `rate` with the given
    /// hop (controls the cross-frame ratio smoothing time constant).
    #[must_use]
    pub fn new(planner: &mut RealFftPlanner<f64>, fft_size: usize, rate: u32, hop: usize) -> Self {
        let bins = fft_size / 2 + 1;
        let nyquist = f64::from(rate) / 2.0;
        let b_hi = bark(nyquist);
        let control_freqs: Vec<f64> = (0..BARK_POINTS)
            .map(|j| b_hi * j as f64 / (BARK_POINTS - 1) as f64)
            .map(|b| bark_inverse(b, nyquist * 1.5))
            .collect();
        let hop_sec = hop as f64 / f64::from(rate);
        let cep_len = bins.next_power_of_two();
        Self {
            bins,
            rate,
            nyquist,
            r2c: planner.plan_fft_forward(cep_len),
            c2r: planner.plan_fft_inverse(cep_len),
            control_freqs,
            prev_ratio_db: vec![0.0; bins],
            smoothing_init: false,
            smoothing_coef: 1.0 - (-hop_sec / 0.012).exp(),
            smooth_width_bins: Self::bark_width_table(bins, rate),
            cep_len,
        }
    }

    /// Per-bin smoothing width in bins for ~0.375 Bark of smoothing.
    fn bark_width_table(bins: usize, rate: u32) -> Vec<usize> {
        let bin_hz = f64::from(rate) / ((bins - 1) * 2) as f64;
        let nyq = f64::from(rate) / 2.0;
        (0..bins)
            .map(|k| {
                let f = (k as f64 * bin_hz).max(1.0);
                let b = bark(f);
                let f_lo = bark_inverse((b - 0.1875).max(0.0), nyq * 2.0);
                let f_hi = bark_inverse(b + 0.1875, nyq * 2.0);
                (((f_hi - f_lo) / bin_hz).round() as usize).max(1)
            })
            .collect()
    }

    /// Control-point frequencies in Hz (diagnostics / UI envelope view).
    #[must_use]
    pub fn control_freqs(&self) -> &[f64] {
        &self.control_freqs
    }

    /// Quefrency lifter cutoff measured in the *padded* cepstral
    /// transform's index units: keep quefrencies that carry envelope
    /// structure, drop the harmonic ripple (which peaks at
    /// `rate / (2·F0)` in a length-`bins` transform, i.e.
    /// `rate / (2·F0) · L / bins` in the length-`L` padded transform).
    /// F0-aware per plan.
    fn lifter_cutoff_padded(&self, f0: Option<f64>) -> usize {
        let q_bins = match f0 {
            Some(f) if (crate::analysis::MIN_F0_HZ..=crate::analysis::MAX_F0_HZ).contains(&f) => {
                0.75 * f64::from(self.rate) / (2.0 * f)
            }
            _ => self.bins as f64 / 16.0,
        };
        let q = q_bins * self.cep_len as f64 / self.bins as f64;
        (q as usize).clamp(8, self.cep_len / 4)
    }

    /// Ceptrally smoothed version of a log-magnitude curve (natural log
    /// units), low-quefrency liftered. The transform runs at the next
    /// power of two above `bins` (257 is prime — Bluestein would make
    /// every frame an order of magnitude slower); the zero padding only
    /// rescales quefrency indices, which [`Self::lifter_cutoff_padded`]
    /// accounts for.
    fn cepstral_smooth(&self, s: &[f64], q_cutoff: usize) -> Vec<f64> {
        let mut input = vec![0.0f64; self.cep_len];
        input[..s.len().min(self.cep_len)].copy_from_slice(&s[..s.len().min(self.cep_len)]);
        let mut spec = self.r2c.make_output_vec();
        self.r2c
            .process(&mut input, &mut spec)
            .expect("pre-sized buffers");
        for b in spec.iter_mut().skip(q_cutoff + 1) {
            *b = Complex64::new(0.0, 0.0);
        }
        let mut out = vec![0.0f64; self.cep_len];
        self.c2r
            .process(&mut spec, &mut out)
            .expect("hermitian by construction");
        let scale = 1.0 / self.cep_len as f64;
        out.iter().map(|v| v * scale).take(s.len()).collect()
    }

    /// Iterative true envelope of a magnitude spectrum, in dB.
    ///
    /// Returns a smooth curve that upper-bounds `20·log10(mag)` and
    /// fills the valleys between harmonics (the physical envelope).
    #[must_use]
    pub fn true_envelope_db(&self, mags: &[f64], f0: Option<f64>) -> Vec<f64> {
        assert_eq!(mags.len(), self.bins, "magnitude/bin mismatch");
        let s: Vec<f64> = mags.iter().map(|m| (m + 1e-12).ln()).collect();
        let q = self.lifter_cutoff_padded(f0);
        let mut env = s.clone();
        for _ in 0..ENVELOPE_ITERATIONS {
            let smooth = self.cepstral_smooth(&env, q);
            for (e, (&sm, &sig)) in env.iter_mut().zip(smooth.iter().zip(&s)) {
                *e = sm.max(sig);
            }
        }
        env.iter()
            .map(|v| v * (20.0 / std::f64::consts::LN_10))
            .collect()
    }

    /// Per-bin multiplicative field (linear gain) implementing the
    /// envelope warp `g`: output envelope ≈ input envelope evaluated at
    /// `f / g`, i.e. formants move by exactly `g`.
    ///
    /// `g == 1` (within [`IDENTITY_EPSILON`]) returns an exact all-ones
    /// field — the stage is a true no-op when neutral.
    #[must_use]
    pub fn warp_field(&mut self, spectrum: &[Complex64], g: f64, f0: Option<f64>) -> Vec<f64> {
        assert_eq!(spectrum.len(), self.bins, "spectrum/bin mismatch");
        if (g - 1.0).abs() < IDENTITY_EPSILON {
            return vec![1.0; self.bins];
        }
        let mags: Vec<f64> = spectrum.iter().map(|b| b.norm()).collect();
        // Voiced frames: sample the envelope at the harmonics (stable for
        // stationary content — per-frame cepstral estimates wobble with
        // the STFT leakage pattern and amplitude-modulate the signal;
        // found by the engine invariant tests). Unvoiced frames keep the
        // cepstral true envelope.
        let voiced_f0 = f0.filter(|f| {
            (crate::analysis::MIN_F0_HZ..=crate::analysis::MAX_F0_HZ).contains(f)
                && self.nyquist > 2.2 * *f
        });
        let (env_db, harmonic_samples) = match voiced_f0 {
            Some(f) => self.harmonic_envelope_with_samples(&mags, f),
            None => (self.true_envelope_db(&mags, f0), Vec::new()),
        };
        let max_db = env_db.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let floor = max_db + ENVELOPE_FLOOR_DB;
        // Per-bin spectrum level (dB) for the empty-space gate below.
        let spec_db: Vec<f64> = mags.iter().map(|m| 20.0 * (m + 1e-12).log10()).collect();
        let spec_max = spec_db.iter().copied().fold(f64::NEG_INFINITY, f64::max);

        // Ratio field construction. Voiced frames compute the ratio
        // *at the harmonics* — the only bins with energy — and
        // log-frequency interpolate to the bin grid: exact warp at the
        // partials, ripple-free by construction (no smoothing needed,
        // which matters: Bark averaging across a Q≈10 formant peak
        // dilutes the ratio by several dB). Unvoiced frames use the
        // per-bin ratio with variable-width Bark smoothing.
        let bin_hz = f64::from(self.rate) / ((self.bins - 1) * 2) as f64;
        let gate_hi = spec_max - 60.0;
        let gate_lo = spec_max - 80.0;

        let fold_src = |src: f64| -> f64 {
            // Envelope value at the (possibly folded) source position.
            if src <= self.nyquist {
                self.eval_env_db(&env_db, src)
            } else {
                let folded = 2.0 * self.nyquist - src;
                if folded >= 0.0 {
                    self.eval_env_db(&env_db, folded) + FOLD_ATTENUATION_DB
                } else {
                    floor + FOLD_ATTENUATION_DB
                }
            }
        };

        let mut ratio_db: Vec<f64> = if !harmonic_samples.is_empty() {
            // --- Voiced: exact per-harmonic ratios -------------------
            let mut r_h: Vec<(f64, f64)> = Vec::with_capacity(harmonic_samples.len());
            for &(f_h, e_h) in &harmonic_samples {
                let w = fold_src(f_h / g).max(floor);
                let o = e_h.max(floor);
                let r = (w - o).clamp(-RATIO_CLAMP_DB, RATIO_CLAMP_DB);
                // Gate on the harmonic's own level.
                let level = self.eval_env_db(&spec_db, f_h);
                let gate = if level >= gate_hi {
                    1.0
                } else if level <= gate_lo {
                    0.0
                } else {
                    (level - gate_lo) / (gate_hi - gate_lo)
                };
                r_h.push((f_h, r * gate));
            }
            (0..self.bins)
                .map(|k| {
                    let f = k as f64 * bin_hz;
                    if f <= r_h[0].0 {
                        r_h[0].1
                    } else if f >= r_h[r_h.len() - 1].0 {
                        r_h[r_h.len() - 1].1
                    } else {
                        let hi = r_h.partition_point(|&(hf, _)| hf < f);
                        let (fa, da) = r_h[hi - 1];
                        let (fb, db) = r_h[hi.min(r_h.len() - 1)];
                        let t = ((f / fa).ln() / (fb / fa).ln()).clamp(0.0, 1.0);
                        da + t * (db - da)
                    }
                })
                .collect()
        } else {
            // --- Unvoiced: per-bin ratio + Bark smoothing -------------
            let mut ratio_db: Vec<f64> = Vec::with_capacity(self.bins);
            for (k, &env) in env_db.iter().enumerate() {
                let f = k as f64 * bin_hz;
                let w = fold_src(f / g).max(floor);
                let o = env.max(floor);
                let r = (w - o).clamp(-RATIO_CLAMP_DB, RATIO_CLAMP_DB);
                // Empty-space gate keyed on the *bin* spectrum level:
                // never reshape spectral silence.
                let level = spec_db[k];
                let gate = if level >= gate_hi {
                    1.0
                } else if level <= gate_lo {
                    0.0
                } else {
                    (level - gate_lo) / (gate_hi - gate_lo)
                };
                ratio_db.push(r * gate);
            }
            smooth_bark(&ratio_db, &self.smooth_width_bins)
        };

        // Cross-frame one-pole (kills residual frame-rate AM). The first
        // engaged frame passes through unfiltered — initialising the
        // state to 0 dB would dilute the first field by the pole
        // coefficient (a 40 ms formant fade-in at stream starts).
        if !self.smoothing_init {
            self.prev_ratio_db.copy_from_slice(&ratio_db);
            self.smoothing_init = true;
        } else {
            for (r, prev) in ratio_db.iter_mut().zip(&mut self.prev_ratio_db) {
                *r = *prev + (*r - *prev) * self.smoothing_coef;
                *prev = *r;
            }
        }

        // Convert to linear gain.
        ratio_db.iter().map(|r| 10.0f64.powf(r / 20.0)).collect()
    }

    /// Harmonic-sampled spectral envelope (dB, per bin): the magnitude
    /// at each harmonic `h·f0` (linear bin interpolation), log-frequency
    /// interpolation between harmonics, flat below the fundamental,
    /// −12 dB/octave rolloff above the top harmonic. Falls back to the
    /// cepstral true envelope when fewer than two harmonics fit below
    /// Nyquist.
    /// The per-bin harmonic envelope plus the raw `(Hz, dB)` harmonic
    /// samples it was built from (used by the voiced warp path).
    fn harmonic_envelope_with_samples(&self, mags: &[f64], f0: f64) -> (Vec<f64>, Vec<(f64, f64)>) {
        let bin_hz = f64::from(self.rate) / ((self.bins - 1) * 2) as f64;
        let mut harmonics: Vec<(f64, f64)> = Vec::new(); // (Hz, dB)
        let mut h = 1usize;
        while f0 * h as f64 <= self.nyquist {
            let f = f0 * h as f64;
            let pos = (f / bin_hz).clamp(0.0, (self.bins - 1) as f64);
            let i = pos.floor() as usize;
            let t = pos - i as f64;
            let j = (i + 1).min(self.bins - 1);
            let m = mags[i] * (1.0 - t) + mags[j] * t;
            harmonics.push((f, 20.0 * (m + 1e-12).log10()));
            h += 1;
        }
        if harmonics.len() < 2 {
            return (self.true_envelope_db(mags, Some(f0)), harmonics);
        }
        let mut env = vec![0.0f64; self.bins];
        for (k, e) in env.iter_mut().enumerate() {
            let f = k as f64 * bin_hz;
            *e = if f <= harmonics[0].0 {
                harmonics[0].1
            } else if f >= harmonics[harmonics.len() - 1].0 {
                let (top_f, top_db) = harmonics[harmonics.len() - 1];
                top_db - 12.0 * (f / top_f).log2().max(0.0)
            } else {
                // log-frequency interpolation between bracketing harmonics
                let hi = harmonics.partition_point(|&(hf, _)| hf < f);
                let (fa, da) = harmonics[hi - 1];
                let (fb, db) = harmonics[hi.min(harmonics.len() - 1)];
                let t = ((f / fa).ln() / (fb / fa).ln()).clamp(0.0, 1.0);
                da + t * (db - da)
            };
        }
        (env, harmonics)
    }

    /// Evaluate the per-bin envelope (dB) at an arbitrary frequency.
    fn eval_env_db(&self, env_db: &[f64], f: f64) -> f64 {
        let bin_hz = f64::from(self.rate) / ((self.bins - 1) * 2) as f64;
        let pos = (f / bin_hz).clamp(0.0, (self.bins - 1) as f64);
        let i = pos.floor() as usize;
        let frac = pos - i as f64;
        let j = (i + 1).min(self.bins - 1);
        env_db[i] * (1.0 - frac) + env_db[j] * frac
    }
}

/// Per-bin moving average with a per-bin radius (Bark-scaled): smooth
/// across the critical band, neverrippling between adjacent bins.
fn smooth_bark(v: &[f64], widths: &[usize]) -> Vec<f64> {
    if v.is_empty() {
        return v.to_vec();
    }
    (0..v.len())
        .map(|i| {
            let w = widths.get(i).copied().unwrap_or(1).max(1);
            let lo = i.saturating_sub(w);
            let hi = (i + w + 1).min(v.len());
            v[lo..hi].iter().sum::<f64>() / (hi - lo) as f64
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stft::StftAnalyzer;
    use crate::testsupport as ts;

    const RATE: u32 = 48_000;
    const N: usize = 2048;

    fn processor() -> (realfft::RealFftPlanner<f64>, FormantProcessor) {
        let mut planner = realfft::RealFftPlanner::new();
        let p = FormantProcessor::new(&mut planner, N, RATE, N / 4);
        (planner, p)
    }

    /// Mid-stream STFT frame spectrum of `sig`.
    fn mid_spectrum(sig: &[f32]) -> Vec<Complex64> {
        let mut planner = realfft::RealFftPlanner::new();
        let mut stft = StftAnalyzer::new(&mut planner, N, N / 4);
        let mut frames = stft.push(sig);
        frames.extend(stft.flush());
        let mid = &frames[frames.len() / 2];
        mid.spectrum.clone()
    }

    #[test]
    fn bark_scale_matches_reference_points() {
        assert!((bark(0.0)).abs() < 1e-9);
        assert!(
            (bark(1000.0) - 8.51).abs() < 0.05,
            "bark(1k)={}",
            bark(1000.0)
        );
        assert!(
            (bark(4000.0) - 17.0).abs() < 0.4,
            "bark(4k)={}",
            bark(4000.0)
        );
        // Monotone up to Nyquist.
        let mut prev = 0.0;
        for f in (0..24_000).step_by(500) {
            let b = bark(f as f64);
            assert!(b >= prev, "bark not monotone at {f}");
            prev = b;
        }
        // Inverse round trip.
        for f in [100.0, 1000.0, 5000.0, 20_000.0] {
            let back = bark_inverse(bark(f), 48_000.0);
            assert!((back - f).abs() < f * 1e-3, "inverse {back} vs {f}");
        }
    }

    #[test]
    fn true_envelope_upper_bounds_and_fills_valleys() {
        let (_, p) = processor();
        // Harmonic-comb magnitude: spikes every 2 bins (dense harmonics),
        // shaped by a broad formant bump at bin 80 (~1.9 kHz).
        let bin_hz = RATE as f64 / N as f64;
        let f1 = 1900.0;
        let mags: Vec<f64> = (0..p.bins)
            .map(|k| {
                let f = k as f64 * bin_hz;
                let spike = if k % 2 == 0 { 1.0 } else { 0.01 };
                let formant = 1.0 / ((1.0 - (f / f1).powi(2)).powi(2) + 0.02).sqrt();
                spike * formant * 0.01
            })
            .collect();
        let env = p.true_envelope_db(&mags, Some(200.0));
        let spec_db: Vec<f64> = mags.iter().map(|m| 20.0 * (m + 1e-12).log10()).collect();
        // Upper bound property.
        for k in 0..p.bins {
            assert!(
                env[k] >= spec_db[k] - 0.05,
                "envelope below spectrum at bin {k}: {:.2} < {:.2}",
                env[k],
                spec_db[k]
            );
        }
        // Valley filling: between two spikes the envelope must sit well
        // above the valley floor (within ~8 dB of the spike level).
        let k_spike = 80;
        let valley = env[k_spike + 1];
        let spike = spec_db[k_spike];
        assert!(
            valley > spike - 8.0,
            "envelope did not fill valleys: {valley:.1} vs spike {spike:.1}"
        );
        // Envelope peak tracks the formant (search 500-4000 Hz).
        let (k_lo, k_hi) = (
            (500.0 / bin_hz) as usize,
            ((4000.0 / bin_hz) as usize).min(p.bins - 1),
        );
        let peak_bin = (k_lo..=k_hi)
            .max_by(|&a, &b| env[a].total_cmp(&env[b]))
            .expect("range");
        let peak_f = peak_bin as f64 * bin_hz;
        assert!(
            (peak_f - f1).abs() < 300.0,
            "envelope peak at {peak_f:.0} Hz, formant at {f1}"
        );
    }

    #[test]
    fn warp_field_moves_envelope_peak_by_g() {
        let (_, mut p) = processor();
        // Vowel with F1 = 730 Hz.
        let sig = ts::formant_vowel(196.0, &[(730.0, 10.0), (1090.0, 10.0)], 0.5, 48_000, RATE);
        let spec = mid_spectrum(&sig);
        let mags: Vec<f64> = spec.iter().map(|b| b.norm()).collect();

        // Harmonic samples before, and after applying the warp field
        // (same estimator both sides — measurement bias cancels).
        let harm_before = p.harmonic_envelope_with_samples(&mags, 196.0).1;
        let field = p.warp_field(&spec, 1.25, Some(196.0));
        let warped_mags: Vec<f64> = spec
            .iter()
            .zip(&field)
            .map(|(b, &g)| b.norm() * g)
            .collect();
        let harm_after = p.harmonic_envelope_with_samples(&warped_mags, 196.0).1;

        let centroid = |h: &[(f64, f64)], lo: f64, hi: f64| {
            let mut num = 0.0f64;
            let mut den = 0.0f64;
            for &(f, db) in h {
                if (lo..=hi).contains(&f) {
                    let pw = 10.0f64.powf(db / 10.0);
                    num += pw * f;
                    den += pw;
                }
            }
            if den > 0.0 {
                num / den
            } else {
                0.5 * (lo + hi)
            }
        };
        let c_in = centroid(&harm_before, 300.0, 1600.0);
        let c_out = centroid(&harm_after, 300.0, 2000.0);
        let expected = c_in * 1.25;
        assert!(
            (c_out - expected).abs() < expected * 0.05,
            "F1 harmonic centroid {c_in:.0} -> {c_out:.0}, expected ~{expected:.0}"
        );
    }

    #[test]
    fn warp_field_identity_is_exact_noop() {
        let (_, mut p) = processor();
        let spec = mid_spectrum(&ts::formant_vowel(
            196.0,
            &[(730.0, 10.0)],
            0.5,
            48_000,
            RATE,
        ));
        let field = p.warp_field(&spec, 1.0, Some(196.0));
        assert!(field.iter().all(|&g| g == 1.0));
        let field = p.warp_field(&spec, 1.0 + 5e-7, Some(196.0));
        assert!(
            field.iter().all(|&g| g == 1.0),
            "near-identity must shortcut"
        );
    }

    #[test]
    fn warp_field_survives_spectral_zeros() {
        let (_, mut p) = processor();
        // Alternating zero bins = brutal spectral zeros.
        let mut spec = vec![Complex64::new(0.0, 0.0); p.bins];
        for k in (0..p.bins).step_by(2) {
            spec[k] = Complex64::new(0.01, 0.0);
        }
        for g in [0.7, 1.4] {
            let field = p.warp_field(&spec, g, None);
            assert!(
                field.iter().all(|v| v.is_finite()),
                "g={g}: non-finite gain"
            );
            let max_g = 10.0f64.powf(RATIO_CLAMP_DB / 20.0) * 1.01;
            assert!(
                field.iter().all(|v| *v <= max_g),
                "g={g}: gain exceeded clamp ({})",
                field.iter().copied().fold(0.0f64, f64::max)
            );
        }
    }

    #[test]
    fn warp_field_folds_beyond_nyquist_without_nan() {
        let (_, mut p) = processor();
        let sig = ts::formant_vowel(
            150.0,
            &[(600.0, 8.0), (2400.0, 8.0), (8000.0, 8.0)],
            0.5,
            48_000,
            RATE,
        );
        let spec = mid_spectrum(&sig);
        // Strong downward warp: most of the spectrum folds.
        let field = p.warp_field(&spec, 0.5, Some(150.0));
        assert!(field.iter().all(|v| v.is_finite()));
        assert!(field.iter().all(|v| *v > 0.0));
    }

    #[test]
    fn magnitude_only_warp_preserves_f0() {
        // Engine invariant #4 at module level: full STFT -> warp -> ISTFT.
        let (_, mut p) = processor();
        let (hop, n) = (N / 4, N);
        let sig = ts::formant_vowel(196.0, &[(730.0, 10.0), (1090.0, 10.0)], 0.5, 48_000, RATE);
        let mut planner = realfft::RealFftPlanner::new();
        let mut stft = StftAnalyzer::new(&mut planner, n, hop);
        let c2r = planner.plan_fft_inverse(n);
        let mut ola = crate::stft::OverlapAdder::new(n, crate::stft::hann_window(n));
        let mut frames = stft.push(&sig);
        frames.extend(stft.flush());
        for f in &frames {
            let field = p.warp_field(&f.spectrum, 1.3, Some(196.0));
            let mut spec: Vec<Complex64> = f
                .spectrum
                .iter()
                .zip(&field)
                .map(|(b, &g)| Complex64::new(b.re * g, b.im * g))
                .collect();
            spec[0].im = 0.0;
            let last = spec.len() - 1;
            spec[last].im = 0.0;
            let mut time = vec![0.0f64; n];
            c2r.process(&mut spec, &mut time).unwrap();
            let scale = 1.0 / n as f64;
            for v in &mut time {
                *v *= scale;
            }
            ola.add_frame_at(&time, f.start);
        }
        let pad = n - hop;
        let mut out = ola.drain(pad + sig.len());
        out.drain(..pad);
        out.truncate(sig.len());
        let (f0, _) = crate::analysis::yin_f0(&out[2000..out.len() - 2000], RATE)
            .expect("pitch lost after warp");
        assert!(
            (f0 - 196.0).abs() < 196.0 * 0.001,
            "warp changed F0: {f0:.2} vs 196.00"
        );
    }
}
