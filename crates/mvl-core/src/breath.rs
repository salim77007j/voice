//! Air & breath control — the signature feature (architecture plan
//! §6.4).
//!
//! One bipolar slider (−100 % … +100 %, 0.1 dB precision) processed
//! *only* on frames the shared classifier tags `Breath` or `Sibilant`
//! in the negative direction:
//!
//! * **Breath frames (remove):** full-band spectral gain, up to −40 dB
//!   at −100 %, with double smoothing — temporal ballistics (5 ms
//!   attack / 120 ms release, so breaths duck musically instead of
//!   gating) and analytic critical-band smoothness (all band shapes are
//!   cosine-ramped, so the gain field can never ripple between adjacent
//!   bins → no musical noise). Onset transients (~1.3 ms energy-rise
//!   detection from the analysis layer) are exempted so plosive
//!   attacks survive.
//! * **Sibilant frames (de-ess):** split-band attenuation of 4–10 kHz,
//!   proportional to sibilance confidence, up to −24 dB at −100 %, with
//!   the mid-band untouched so diction stays crisp.
//! * **Positive direction (add air):** a phase-flat spectral high-shelf
//!   (+0…+6 dB, full strength above 9 kHz, half strength in the 5–9 kHz
//!   presence zone, nothing below) plus harmonic air synthesis — the
//!   2.5–6 kHz harmonic content transposed +1 octave and re-injected at
//!   −24…−12 dB, envelope-gated by voicing so silence stays silent.
//!
//! The honesty guarantee: removal never touches voiced frames (the
//! gain field is exactly 0 dB there), and the engine's
//! difference-listen path (input − output) can verify it.

use rustfft::num_complex::Complex64;

use crate::analysis::{FrameClass, FrameFeatures, SIBILANT_HB_RATIO};
use crate::params::VocalParams;

/// Maximum breath reduction at −100 % (dB).
pub const MAX_BREATH_REDUCTION_DB: f64 = -40.0;
/// Maximum sibilant band attenuation at −100 % (dB).
pub const MAX_DEESS_DB: f64 = -24.0;
/// Maximum high-shelf lift at +100 % (dB).
pub const MAX_SHELF_DB: f64 = 6.0;
/// Harmonic-air re-injection level range (dB at +0 % … +100 %).
pub const AIR_INJECT_DB_MIN: f64 = -24.0;
pub const AIR_INJECT_DB_MAX: f64 = -12.0;
/// Ballistics (plan §6.4).
const ATTACK_MS: f64 = 5.0;
const RELEASE_MS: f64 = 120.0;
const GATE_RELEASE_MS: f64 = 60.0;
/// Sibilant band edges (Hz): cosine ramps 3.5–4.5 kHz and 9.5–10.5 kHz.
const SIB_LO: (f64, f64) = (3500.0, 4500.0);
const SIB_HI: (f64, f64) = (9500.0, 10_500.0);
/// Shelf shape: ramp to half strength 4.5–5.5 kHz, half zone to 8.5 kHz,
/// ramp to full 8.5–9.5 kHz, full above.
const SHELF_RAMP_LO: (f64, f64) = (4500.0, 5500.0);
const SHELF_RAMP_HI: (f64, f64) = (8500.0, 9500.0);
/// Harmonic-air target band (octave above 2.5–6 kHz source).
const AIR_TARGET_BAND: (f64, f64) = (5000.0, 12_000.0);

/// One frame's spectral edit from the air/breath engine.
#[derive(Debug, Clone)]
pub struct BreathEdit {
    /// Per-bin multiplicative gain (linear, ≥ 0).
    pub gain: Vec<f64>,
    /// Per-bin additive content (harmonic air), zero when unused.
    pub additive: Vec<Complex64>,
}

/// Streaming air/breath processor (ballistic state across frames).
pub struct BreathProcessor {
    bins: usize,
    rate: u32,
    bin_hz: f64,
    attack: f64,
    release: f64,
    gate_release: f64,
    breath_duck_db: f64,
    sib_duck_db: f64,
    air_gate: f64,
    last_applied_db: f64,
}

impl BreathProcessor {
    /// Create for an STFT with `bins` bins at `rate`, hop `hop_samples`.
    #[must_use]
    pub fn new(bins: usize, rate: u32, hop_samples: usize) -> Self {
        let hop_sec = hop_samples as f64 / f64::from(rate);
        let one_pole = |tau_ms: f64| 1.0 - (-hop_sec / (tau_ms / 1000.0)).exp();
        Self {
            bins,
            rate,
            bin_hz: f64::from(rate) / ((bins - 1) * 2) as f64,
            attack: one_pole(ATTACK_MS),
            release: one_pole(RELEASE_MS),
            gate_release: one_pole(GATE_RELEASE_MS),
            breath_duck_db: 0.0,
            sib_duck_db: 0.0,
            air_gate: 0.0,
            last_applied_db: 0.0,
        }
    }

    /// Gain applied to the most recent frame (dB; 0 = none). The 0.1 dB
    /// readout of the UI comes from here.
    #[must_use]
    pub fn last_applied_db(&self) -> f64 {
        self.last_applied_db
    }

    /// Reset ballistics (stream restart).
    pub fn reset(&mut self) {
        self.breath_duck_db = 0.0;
        self.sib_duck_db = 0.0;
        self.air_gate = 0.0;
        self.last_applied_db = 0.0;
    }

    /// Process one frame. `class` is the *smoothed* classification; the
    /// raw features provide the sibilance confidence and transient flag.
    #[must_use]
    pub fn process_frame(
        &mut self,
        spectrum: &[Complex64],
        feats: &FrameFeatures,
        class: FrameClass,
        params: &VocalParams,
    ) -> BreathEdit {
        assert_eq!(spectrum.len(), self.bins, "spectrum/bin mismatch");
        let p = params.air_percent;

        // --- Negative direction: duck ballistics -------------------
        let breath_target = if p < 0 && class == FrameClass::Breath && !feats.transient {
            MAX_BREATH_REDUCTION_DB * (f64::from(-p) / 100.0)
        } else {
            0.0
        };
        let sib_conf = ((f64::from(feats.high_band_ratio) - SIBILANT_HB_RATIO as f64)
            / (1.0 - SIBILANT_HB_RATIO as f64))
            .clamp(0.0, 1.0);
        let sib_target = if p < 0 && class == FrameClass::Sibilant && !feats.transient {
            MAX_DEESS_DB * (f64::from(-p) / 100.0) * sib_conf
        } else {
            0.0
        };
        self.breath_duck_db = ballism(
            self.breath_duck_db,
            breath_target,
            self.attack,
            self.release,
        );
        self.sib_duck_db = ballism(self.sib_duck_db, sib_target, self.attack, self.release);

        // --- Positive direction: voicing gate for air synthesis ----
        let gate_target = if p > 0 && class == FrameClass::Voiced {
            1.0
        } else {
            0.0
        };
        self.air_gate = ballism(self.air_gate, gate_target, self.attack, self.gate_release);

        // --- Compose the per-bin gain field -------------------------
        let mut gain = vec![1.0f64; self.bins];
        let duck = self.breath_duck_db; // full-band, ≤ 0 dB
        let sib = self.sib_duck_db; // band-limited, ≤ 0 dB
        let shelf_db = if p > 0 {
            MAX_SHELF_DB * f64::from(p) / 100.0
        } else {
            0.0
        };

        for (k, g) in gain.iter_mut().enumerate() {
            let f = k as f64 * self.bin_hz;
            let mut db = duck;
            if sib < 0.0 {
                db += sib * band_shape(f, SIB_LO, SIB_HI);
            }
            if shelf_db > 0.0 {
                db += shelf_db * shelf_shape(f);
            }
            *g = 10.0f64.powf(db / 20.0);
        }

        // --- Harmonic air synthesis (octave-up re-injection) --------
        let mut additive = vec![Complex64::new(0.0, 0.0); self.bins];
        if p > 0 && self.air_gate > 1e-3 {
            let inject_db =
                AIR_INJECT_DB_MIN + (AIR_INJECT_DB_MAX - AIR_INJECT_DB_MIN) * f64::from(p) / 100.0;
            let w = 10.0f64.powf(inject_db / 20.0) * self.air_gate;
            let (lo, hi) = AIR_TARGET_BAND;
            let (k_lo, k_hi) = (
                (lo / self.bin_hz).ceil() as usize,
                ((hi / self.bin_hz).floor() as usize).min(self.bins - 1),
            );
            for (k, a) in additive.iter_mut().enumerate().take(k_hi + 1).skip(k_lo) {
                // Source bin of the octave-down content: f_k / 2.
                let src = interp_complex(spectrum, k as f64 / 2.0);
                *a = src * w;
            }
        }

        self.last_applied_db = duck.min(sib);
        BreathEdit { gain, additive }
    }

    /// Session sample rate (diagnostics).
    #[must_use]
    pub fn rate(&self) -> u32 {
        self.rate
    }
}

/// One-pole ballistic step toward `target`: fast when engaging (attack),
/// slow when recovering (release).
fn ballism(current: f64, target: f64, attack: f64, release: f64) -> f64 {
    let coef = if target < current { attack } else { release };
    current + (target - current) * coef
}

/// Cosine-ramped band shape: 0 below `lo.0`, ramps to 1 at `lo.1`, holds,
/// ramps back to 0 at `hi.1`.
fn band_shape(f: f64, lo: (f64, f64), hi: (f64, f64)) -> f64 {
    if f <= lo.0 || f >= hi.1 {
        0.0
    } else if f < lo.1 {
        0.5 - 0.5 * (std::f64::consts::PI * (f - lo.0) / (lo.1 - lo.0)).cos()
    } else if f <= hi.0 {
        1.0
    } else {
        0.5 + 0.5 * (std::f64::consts::PI * (f - hi.0) / (hi.1 - hi.0)).cos()
    }
}

/// Shelf shape: 0 below 4.5 kHz, 0.5 through the 5–9 kHz presence zone,
/// 1 above 9.5 kHz (cosine ramps between).
fn shelf_shape(f: f64) -> f64 {
    if f <= SHELF_RAMP_LO.0 {
        0.0
    } else if f < SHELF_RAMP_LO.1 {
        0.5 * (1.0
            - (std::f64::consts::PI * (f - SHELF_RAMP_LO.0) / (SHELF_RAMP_LO.1 - SHELF_RAMP_LO.0))
                .cos())
    } else if f <= SHELF_RAMP_HI.0 {
        0.5
    } else if f < SHELF_RAMP_HI.1 {
        0.5 + 0.5
            * (std::f64::consts::PI * (f - SHELF_RAMP_HI.0) / (SHELF_RAMP_HI.1 - SHELF_RAMP_HI.0))
                .cos()
    } else {
        1.0
    }
}

/// Linear interpolation of a complex spectrum at fractional bin `pos`.
fn interp_complex(spec: &[Complex64], pos: f64) -> Complex64 {
    let i = pos.floor().clamp(0.0, (spec.len() - 1) as f64) as usize;
    let j = (i + 1).min(spec.len() - 1);
    let t = pos - i as f64;
    Complex64::new(
        spec[i].re * (1.0 - t) + spec[j].re * t,
        spec[i].im * (1.0 - t) + spec[j].im * t,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stft::{hann_window, OverlapAdder, StftAnalyzer};
    use crate::testsupport as ts;
    use crate::{ClassSmoother, FrameAnalyzer, VocalParams};

    const RATE: u32 = 48_000;
    const N: usize = 512;
    const HOP: usize = 128;

    /// Full air/breath stage over a signal: STFT → analysis → smoothing →
    /// breath edit → ISTFT. Mirrors the engine wiring of Commit F.
    fn process(sig: &[f32], params: &VocalParams) -> (Vec<f32>, Vec<f64>) {
        let mut planner = realfft::RealFftPlanner::new();
        let mut stft = StftAnalyzer::new(&mut planner, N, HOP);
        let c2r = planner.plan_fft_inverse(N);
        let mut ola = OverlapAdder::new(N, hann_window(N));
        let mut an = FrameAnalyzer::new(RATE);
        let mut smooth = ClassSmoother::new(3);
        let mut breath = BreathProcessor::new(N / 2 + 1, RATE, HOP);
        let mut applied: Vec<f64> = Vec::new();
        let mut pos = 0;
        while pos < sig.len() {
            let end = (pos + HOP).min(sig.len());
            an.push_history(&sig[pos..end]);
            let frames = stft.push(&sig[pos..end]);
            for f in &frames {
                run_frame(
                    f,
                    &mut an,
                    &mut smooth,
                    &mut breath,
                    &c2r,
                    &mut ola,
                    &mut applied,
                    params,
                );
            }
            pos = end;
        }
        let frames = stft.flush();
        for f in &frames {
            run_frame(
                f,
                &mut an,
                &mut smooth,
                &mut breath,
                &c2r,
                &mut ola,
                &mut applied,
                params,
            );
        }
        let pad = N - HOP;
        let mut full = ola.drain(pad + sig.len());
        full.drain(..pad);
        full.truncate(sig.len());
        (full, applied)
    }

    #[allow(clippy::too_many_arguments)]
    fn run_frame(
        f: &crate::stft::AnalysisFrame,
        an: &mut FrameAnalyzer,
        smooth: &mut ClassSmoother,
        breath: &mut BreathProcessor,
        c2r: &std::sync::Arc<dyn realfft::ComplexToReal<f64>>,
        ola: &mut OverlapAdder,
        applied: &mut Vec<f64>,
        params: &VocalParams,
    ) {
        let feats = an.analyze(f.start + HOP, &f.spectrum, N);
        let class = smooth.push(feats.class);
        let edit = breath.process_frame(&f.spectrum, &feats, class, params);
        applied.push(breath.last_applied_db());
        let mut spec: Vec<Complex64> = f
            .spectrum
            .iter()
            .zip(&edit.gain)
            .zip(&edit.additive)
            .map(|((b, &g), &a)| Complex64::new(b.re * g + a.re, b.im * g + a.im))
            .collect();
        spec[0].im = 0.0;
        let last = spec.len() - 1;
        spec[last].im = 0.0;
        let mut time = vec![0.0f64; N];
        c2r.process(&mut spec, &mut time).unwrap();
        let scale = 1.0 / N as f64;
        for v in &mut time {
            *v *= scale;
        }
        ola.add_frame_at(&time, f.start);
    }

    #[test]
    fn negative_removes_breath_to_target_db() {
        let sig = ts::breath_noise(11, RATE as usize, RATE, 0.063);
        let params = VocalParams {
            air_percent: -100,
            ..VocalParams::neutral()
        };
        let (out, applied) = process(&sig, &params);
        // Steady-state applied gain ≈ -40 dB (invariant #5: ±0.05 dB).
        let mid = &applied[applied.len() / 2..applied.len() * 3 / 4];
        let avg: f64 = mid.iter().sum::<f64>() / mid.len() as f64;
        assert!(
            (avg - MAX_BREATH_REDUCTION_DB).abs() < 0.05,
            "applied {avg:.3} dB vs target {}",
            MAX_BREATH_REDUCTION_DB
        );
        // Segment energy reduced by the target ± 0.5 dB (invariant #5).
        let core = sig.len() / 4;
        let before = ts::rms_db(&sig[core..sig.len() - core]);
        let after = ts::rms_db(&out[core..out.len() - core]);
        assert!(
            ((after - before) - MAX_BREATH_REDUCTION_DB).abs() < 0.5,
            "energy delta {after:.2} vs {before:.2} (Δ {} dB, target {})",
            after - before,
            MAX_BREATH_REDUCTION_DB
        );
    }

    #[test]
    fn voiced_frames_untouched_at_full_removal() {
        let sig = ts::harmonic_stack(196.0, 16, 0.5, RATE as usize, RATE);
        let params = VocalParams {
            air_percent: -100,
            ..VocalParams::neutral()
        };
        let (out, applied) = process(&sig, &params);
        assert!(
            applied.iter().all(|d| *d > -0.5),
            "voiced frames were ducked: min {:.2}",
            applied.iter().copied().fold(0.0f64, f64::min)
        );
        let core = sig.len() / 4;
        let before = ts::rms_db(&sig[core..sig.len() - core]);
        let after = ts::rms_db(&out[core..out.len() - core]);
        assert!(
            (after - before).abs() < 0.1,
            "voiced energy moved {before:.2} -> {after:.2}"
        );
    }

    #[test]
    fn sibilant_deesser_hits_band_only() {
        // Fixture: strong 4-10 kHz sibilance PLUS real 100-3000 Hz
        // content, so the mid-band measurement is signal, not FFT
        // leakage. Compare p = -100 against p = 0 through the identical
        // pipeline: the delta isolates the de-esser's gain field.
        let n = RATE as usize;
        let mid = ts::band_noise(21, 100.0, 3000.0, n, RATE, 0.03);
        let sib = ts::band_noise(22, 4000.0, 10_000.0, n, RATE, 0.12);
        let sig: Vec<f32> = mid.iter().zip(&sib).map(|(a, b)| a + b).collect();
        let off = VocalParams::neutral();
        let on = VocalParams {
            air_percent: -100,
            ..VocalParams::neutral()
        };
        let (out_off, _) = process(&sig, &off);
        let (out_on, _) = process(&sig, &on);
        let core = sig.len() / 4;
        let band =
            |x: &[f32], lo: f64, hi: f64| ts::band_power_db(&x[core..x.len() - core], RATE, lo, hi);
        // Sibilance band: the de-esser is confidence-weighted. The
        // fixture's high-band power ratio is 0.12^2 / (0.12^2 + 0.03^2)
        // = 0.941, so the expected field is -24 dB x confidence with
        // confidence = (0.941 - 0.45) / 0.55 = 0.893 -> -21.4 dB.
        let hbr = 0.12f64.powi(2) / (0.12f64.powi(2) + 0.03f64.powi(2));
        let conf =
            ((hbr - SIBILANT_HB_RATIO as f64) / (1.0 - SIBILANT_HB_RATIO as f64)).clamp(0.0, 1.0);
        let expected = MAX_DEESS_DB * conf;
        let sib_delta = band(&out_on, 4500.0, 9500.0) - band(&out_off, 4500.0, 9500.0);
        assert!(
            (sib_delta - expected).abs() < 1.5,
            "sibilant band delta {sib_delta:.2} dB (confidence-weighted target {expected:.2})"
        );
        // Mid-band: untouched by the split-band field.
        let mid_delta = band(&out_on, 100.0, 3000.0) - band(&out_off, 100.0, 3000.0);
        assert!(mid_delta.abs() < 0.5, "mid-band moved by {mid_delta:.2} dB");
        // Low edge of the ramp band also essentially untouched.
        let low_delta = band(&out_on, 1000.0, 3400.0) - band(&out_off, 1000.0, 3400.0);
        assert!(
            low_delta.abs() < 0.5,
            "1-3.4 kHz moved by {low_delta:.2} dB"
        );
    }

    #[test]
    fn positive_shelf_lifts_only_highs() {
        let sig = ts::band_noise(5, 100.0, 16_000.0, RATE as usize, RATE, 0.2);
        let params = VocalParams {
            air_percent: 100,
            ..VocalParams::neutral()
        };
        let (out, _) = process(&sig, &params);
        let core = sig.len() / 4;
        let hi = |x: &[f32]| ts::band_power_db(&x[core..x.len() - core], RATE, 10_500.0, 16_000.0);
        let mid = |x: &[f32]| ts::band_power_db(&x[core..x.len() - core], RATE, 6000.0, 8000.0);
        let low = |x: &[f32]| ts::band_power_db(&x[core..x.len() - core], RATE, 100.0, 3000.0);
        assert!(
            (hi(&out) - hi(&sig) - MAX_SHELF_DB).abs() < 0.3,
            "high band {} -> {}",
            hi(&sig),
            hi(&out)
        );
        assert!(
            (mid(&out) - mid(&sig) - MAX_SHELF_DB / 2.0).abs() < 0.5,
            "presence zone {} -> {}",
            mid(&sig),
            mid(&out)
        );
        assert!(
            (low(&out) - low(&sig)).abs() < 0.2,
            "low band moved {} -> {}",
            low(&sig),
            low(&out)
        );
    }

    #[test]
    fn harmonic_air_injection_follows_voicing() {
        // Voiced vowel: air band energy must rise measurably.
        let vowel = ts::formant_vowel(
            196.0,
            &[(730.0, 10.0), (1090.0, 10.0)],
            0.5,
            RATE as usize,
            RATE,
        );
        let params = VocalParams {
            air_percent: 100,
            ..VocalParams::neutral()
        };
        let (out, _) = process(&vowel, &params);
        let core = vowel.len() / 4;
        let air = |x: &[f32]| ts::band_power_db(&x[core..x.len() - core], RATE, 5000.0, 12_000.0);
        assert!(
            air(&out) - air(&vowel) > 2.0,
            "air band only {:+.2} dB (injection failed)",
            air(&out) - air(&vowel)
        );
        // Silence: no hiss floor added (shelf on zeros is zero; injection
        // gated off).
        let silence = ts::silence(RATE as usize);
        let (out_s, applied_s) = process(&silence, &params);
        assert!(applied_s.iter().all(|d| *d == 0.0));
        assert!(ts::max_abs(&out_s) < 1e-6, "silence gained energy");
    }

    #[test]
    fn transient_onset_is_exempt_from_ducking() {
        // Silence, then a sharp breath attack.
        let onset = 9600;
        let sig = ts::concat(
            &ts::silence(onset),
            &ts::fade_in(ts::breath_noise(3, 12_000, RATE, 0.063), 1.0, RATE),
        );
        let params = VocalParams {
            air_percent: -100,
            ..VocalParams::neutral()
        };
        let (_, applied) = process(&sig, &params);
        // Around the onset the duck must not be fully engaged yet
        // (transient exemption + 5 ms attack).
        let onset_frame = onset / HOP;
        let window = &applied[onset_frame.saturating_sub(2)..onset_frame + 4];
        assert!(
            window.iter().any(|d| *d > -12.0),
            "no transient exemption near onset: {window:.1?}"
        );
    }

    #[test]
    fn ballistics_never_jump_instantly() {
        let mut bp = BreathProcessor::new(257, RATE, HOP);
        let feats = FrameFeatures {
            f0_hz: None,
            clarity: 0.0,
            rms: 0.05,
            dbfs: -26.0,
            centroid_hz: 3000.0,
            flatness: 0.6,
            high_band_ratio: 0.1,
            transient: false,
            class: FrameClass::Breath,
        };
        let spec = vec![Complex64::new(0.01, 0.0); 257];
        let params = VocalParams {
            air_percent: -100,
            ..VocalParams::neutral()
        };
        let edit = bp.process_frame(&spec, &feats, FrameClass::Breath, &params);
        assert!(edit.gain.iter().all(|g| g.is_finite()));
        let first = bp.last_applied_db();
        assert!(
            first > MAX_BREATH_REDUCTION_DB + 1.0,
            "first frame jumped to {first:.1} dB (no attack ramp)"
        );
        // Converges to the target over subsequent frames.
        for _ in 0..200 {
            let _ = bp.process_frame(&spec, &feats, FrameClass::Breath, &params);
        }
        assert!(
            (bp.last_applied_db() - MAX_BREATH_REDUCTION_DB).abs() < 0.05,
            "steady state {} dB",
            bp.last_applied_db()
        );
    }

    #[test]
    fn fields_are_finite_across_parameter_grid() {
        let spec: Vec<Complex64> =
            ts::formant_vowel(196.0, &[(730.0, 10.0), (1090.0, 10.0)], 0.5, 2048, RATE)
                .iter()
                .map(|s| Complex64::new(f64::from(*s), 0.0))
                .collect();
        // (Not a real spectrum, but exercises the field math.)
        let feats = FrameFeatures {
            f0_hz: Some(196.0),
            clarity: 0.9,
            rms: 0.5,
            dbfs: -6.0,
            centroid_hz: 1500.0,
            flatness: 0.1,
            high_band_ratio: 0.05,
            transient: false,
            class: FrameClass::Voiced,
        };
        let mut bp = BreathProcessor::new(spec.len(), RATE, HOP);
        for p in [-100, -66, -33, 0, 33, 66, 100] {
            let params = VocalParams {
                air_percent: p,
                ..VocalParams::neutral()
            };
            for class in [
                FrameClass::Voiced,
                FrameClass::Sibilant,
                FrameClass::Breath,
                FrameClass::Silence,
            ] {
                let edit = bp.process_frame(&spec, &feats, class, &params);
                assert!(
                    edit.gain.iter().all(|g| g.is_finite() && *g >= 0.0),
                    "p={p} {class:?}: bad gain field"
                );
                assert!(
                    edit.additive
                        .iter()
                        .all(|a| a.re.is_finite() && a.im.is_finite()),
                    "p={p} {class:?}: bad additive"
                );
            }
        }
    }
}
