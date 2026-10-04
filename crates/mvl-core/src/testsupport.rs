//! Deterministic golden-signal generators and offline measurement helpers.
//!
//! Public (not `#[cfg(test)]`) because the golden-fixture tests in
//! `mvl-io` and the sample generator of `mvl-app` reuse the exact same
//! signals as the unit tests here — one source of truth for every "known
//! input" in the test suite (architecture plan §10.2).
//!
//! Everything is deterministic (seeded LCG noise, fixed-phase synthesis):
//! the same call always yields bit-identical signals on every platform.

use std::f64::consts::PI;

use realfft::RealFftPlanner;

/// Pure sine.
#[must_use]
pub fn sine(freq: f64, amp: f64, frames: usize, rate: u32) -> Vec<f32> {
    let w = 2.0 * PI * freq / f64::from(rate);
    (0..frames)
        .map(|i| (amp * (w * i as f64).sin()) as f32)
        .collect()
}

/// Digital silence.
#[must_use]
pub fn silence(frames: usize) -> Vec<f32> {
    vec![0.0; frames]
}

/// Seeded LCG uniform values in `[0, 1)`.
#[must_use]
pub fn lcg(seed: u64, n: usize) -> Vec<f64> {
    let mut s = seed;
    (0..n)
        .map(|_| {
            s = s
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (s >> 11) as f64 / (1u64 << 53) as f64
        })
        .collect()
}

/// Seeded white noise in `[-1, 1)`.
#[must_use]
pub fn white_noise(seed: u64, frames: usize) -> Vec<f32> {
    lcg(seed, frames)
        .into_iter()
        .map(|v| (v * 2.0 - 1.0) as f32)
        .collect()
}

/// Second-order resonance magnitude at frequency `f` for a formant at
/// `fc` with quality `q` (used by [`formant_vowel`]).
#[must_use]
fn resonance(f: f64, fc: f64, q: f64) -> f64 {
    let r = f / fc;
    let denom = ((1.0 - r * r).powi(2) + (r / q).powi(2)).sqrt();
    1.0 / denom.max(1e-12)
}

/// Harmonic stack with a `1/h` glottal-style tilt.
#[must_use]
pub fn harmonic_stack(f0: f64, harmonics: usize, amp: f64, frames: usize, rate: u32) -> Vec<f32> {
    let nyq = f64::from(rate) / 2.0;
    let mut out = vec![0.0f64; frames];
    for h in 1..=harmonics {
        let f = f0 * h as f64;
        if f >= 0.9 * nyq {
            break;
        }
        let g = amp / h as f64;
        let w = 2.0 * PI * f / f64::from(rate);
        for (i, o) in out.iter_mut().enumerate() {
            *o += g * (w * i as f64).sin();
        }
    }
    normalize_peak(out, amp)
}

/// Additive formant-synthesised vowel: harmonic stack shaped by 2-pole
/// resonances at the given `(centre Hz, Q)` formants.
///
/// The result has objectively known formant positions, which the formant
/// engine's tests measure via [`band_peak_freq`].
#[must_use]
pub fn formant_vowel(
    f0: f64,
    formants: &[(f64, f64)],
    amp: f64,
    frames: usize,
    rate: u32,
) -> Vec<f32> {
    let nyq = f64::from(rate) / 2.0;
    let mut out = vec![0.0f64; frames];
    let mut h = 1usize;
    while f0 * (h as f64) < 0.9 * nyq {
        let f = f0 * (h as f64);
        let mut g = 1.0 / (h as f64);
        for &(fc, q) in formants {
            g *= resonance(f, fc, q);
        }
        let w = 2.0 * PI * f / f64::from(rate);
        for (i, o) in out.iter_mut().enumerate() {
            *o += g * (w * i as f64).sin();
        }
        h += 1;
    }
    normalize_peak(out, amp)
}

/// Scale `sig` so its peak magnitude equals `amp`.
fn normalize_peak(sig: Vec<f64>, amp: f64) -> Vec<f32> {
    let peak = sig.iter().fold(0.0f64, |m, v| m.max(v.abs()));
    if peak > 0.0 {
        let g = amp / peak;
        sig.iter().map(|v| (v * g) as f32).collect()
    } else {
        vec![0.0; sig.len()]
    }
}

/// FFT-domain spectral shaping: multiply each bin's magnitude by
/// `gain(freq)` (DC included), then normalise RMS to `rms_target`.
fn spectral_shape(
    seed: u64,
    frames: usize,
    rate: u32,
    gain: impl Fn(f64) -> f64,
    rms_target: f64,
) -> Vec<f32> {
    assert!(frames >= 16, "spectral shaping needs a reasonable window");
    let raw = lcg(seed, frames);
    let mut planner = RealFftPlanner::new();
    let r2c = planner.plan_fft_forward(frames);
    let c2r = planner.plan_fft_inverse(frames);
    let mut input: Vec<f64> = raw.iter().map(|v| v * 2.0 - 1.0).collect();
    let mut spec = r2c.make_output_vec();
    r2c.process(&mut input, &mut spec)
        .expect("lengths pre-allocated");
    let bin_hz = f64::from(rate) / frames as f64;
    for (k, b) in spec.iter_mut().enumerate() {
        let g = gain(k as f64 * bin_hz);
        b.re *= g;
        b.im *= g;
    }
    let mut out = vec![0.0f64; frames];
    c2r.process(&mut spec, &mut out)
        .expect("lengths pre-allocated");
    let scale = frames as f64;
    for v in &mut out {
        *v /= scale;
    }
    // Trim edge ringing and normalise RMS.
    let core = &out[64..frames - 64];
    let rms = (core.iter().map(|v| v * v).sum::<f64>() / core.len() as f64).sqrt();
    let g = if rms > 1e-12 { rms_target / rms } else { 1.0 };
    out.iter().map(|v| (v * g) as f32).collect()
}

/// Band-limited noise in `[lo, hi]` Hz at the given RMS level.
#[must_use]
pub fn band_noise(
    seed: u64,
    lo: f64,
    hi: f64,
    frames: usize,
    rate: u32,
    rms_target: f64,
) -> Vec<f32> {
    spectral_shape(
        seed,
        frames,
        rate,
        |f| if (lo..=hi).contains(&f) { 1.0 } else { 0.0 },
        rms_target,
    )
}

/// Synthetic breath: broadband aspiration noise, 250 Hz … 0.85·Nyquist,
/// with a −6 dB/octave amplitude tilt above 1 kHz (the spectral shape of
/// aspiration through a constriction), at the given RMS level.
#[must_use]
pub fn breath_noise(seed: u64, frames: usize, rate: u32, rms_target: f64) -> Vec<f32> {
    let hi = 0.85 * f64::from(rate) / 2.0;
    spectral_shape(
        seed,
        frames,
        rate,
        |f| {
            if f < 250.0 || f > hi {
                0.0
            } else if f <= 1000.0 {
                1.0
            } else {
                (f / 1000.0).recip()
            }
        },
        rms_target,
    )
}

/// Synthetic sibilance: noise concentrated in 4–10 kHz at the given RMS
/// level (the /s/ energy region).
#[must_use]
pub fn sibilant_noise(seed: u64, frames: usize, rate: u32, rms_target: f64) -> Vec<f32> {
    spectral_shape(
        seed,
        frames,
        rate,
        |f| {
            if (4000.0..=10_000.0).contains(&f) {
                1.0
            } else {
                0.0
            }
        },
        rms_target,
    )
}

/// Linear fade-in over the first `ms` milliseconds (keeps length).
#[must_use]
pub fn fade_in(mut sig: Vec<f32>, ms: f64, rate: u32) -> Vec<f32> {
    let n = (ms * f64::from(rate) / 1000.0).round() as usize;
    if n > 0 && !sig.is_empty() {
        let n = n.min(sig.len());
        for (i, v) in sig.iter_mut().take(n).enumerate() {
            *v *= i as f32 / n as f32;
        }
    }
    sig
}

/// Concatenate two signals.
#[must_use]
pub fn concat(a: &[f32], b: &[f32]) -> Vec<f32> {
    let mut out = Vec::with_capacity(a.len() + b.len());
    out.extend_from_slice(a);
    out.extend_from_slice(b);
    out
}

/// RMS over a slice.
#[must_use]
pub fn rms(x: &[f32]) -> f64 {
    if x.is_empty() {
        return 0.0;
    }
    let s: f64 = x
        .iter()
        .map(|v| {
            let d = f64::from(*v);
            d * d
        })
        .sum();
    (s / x.len() as f64).sqrt()
}

/// RMS in dBFS (`20·log10`), `-inf`-safe.
#[must_use]
pub fn rms_db(x: &[f32]) -> f64 {
    20.0 * (rms(x) + 1e-12).log10()
}

/// Maximum absolute sample value.
#[must_use]
pub fn max_abs(x: &[f32]) -> f32 {
    x.iter().fold(0.0f32, |m, v| m.max(v.abs()))
}

/// Frequency of the dominant spectral peak within `[lo, hi]` Hz, measured
/// with one FFT over the whole (stationary) slice. Resolution ≈ rate/len;
/// with ≥ 0.25 s of signal this is a few Hz — precise enough for the
/// formant/pitch invariants.
#[must_use]
pub fn band_peak_freq(x: &[f32], rate: u32, lo: f64, hi: f64) -> Option<f64> {
    if x.len() < 16 {
        return None;
    }
    let mut planner = RealFftPlanner::new();
    let r2c = planner.plan_fft_forward(x.len());
    let mut input: Vec<f64> = x.iter().map(|v| f64::from(*v)).collect();
    let mut spec = r2c.make_output_vec();
    r2c.process(&mut input, &mut spec)
        .expect("lengths pre-allocated");
    let bin_hz = f64::from(rate) / x.len() as f64;
    let (k_lo, k_hi) = (
        (lo / bin_hz).ceil().max(1.0) as usize,
        (hi / bin_hz).floor() as usize,
    );
    let mut best: Option<(f64, usize)> = None;
    for (k, b) in spec
        .iter()
        .enumerate()
        .take(k_hi.min(spec.len().saturating_sub(1)) + 1)
        .skip(k_lo)
    {
        let m = b.norm();
        if best.map_or(true, |(bm, _)| m > bm) {
            best = Some((m, k));
        }
    }
    best.map(|(_, k)| k as f64 * bin_hz)
}

/// Total energy in `[lo, hi]` Hz, in dB (10·log10 of FFT band power,
/// normalised by transform length — consistent for before/after
/// comparisons).
#[must_use]
pub fn band_power_db(x: &[f32], rate: u32, lo: f64, hi: f64) -> f64 {
    if x.is_empty() {
        return -120.0;
    }
    let mut planner = RealFftPlanner::new();
    let r2c = planner.plan_fft_forward(x.len());
    let mut input: Vec<f64> = x.iter().map(|v| f64::from(*v)).collect();
    let mut spec = r2c.make_output_vec();
    r2c.process(&mut input, &mut spec)
        .expect("lengths pre-allocated");
    let bin_hz = f64::from(rate) / x.len() as f64;
    let k_lo = ((lo / bin_hz).ceil() as usize).max(1);
    let k_hi = ((hi / bin_hz).floor() as usize).min(spec.len().saturating_sub(1));
    let mut sum = 0.0f64;
    for b in &spec[k_lo..=k_hi] {
        sum += b.norm_sqr();
    }
    let n = x.len() as f64;
    10.0 * (sum / (n * n) + 1e-30).log10()
}

/// Energy-weighted spectral centroid within `[lo, hi]` Hz (one FFT over
/// the whole slice). A robust envelope-position metric: it moves exactly
/// with the spectral envelope's displacement, immune to harmonic
/// peak-picking ambiguity.
#[must_use]
pub fn band_centroid(x: &[f32], rate: u32, lo: f64, hi: f64) -> f64 {
    if x.is_empty() {
        return 0.0;
    }
    let mut planner = RealFftPlanner::new();
    let r2c = planner.plan_fft_forward(x.len());
    let mut input: Vec<f64> = x.iter().map(|v| f64::from(*v)).collect();
    let mut spec = r2c.make_output_vec();
    r2c.process(&mut input, &mut spec)
        .expect("lengths pre-allocated");
    let bin_hz = f64::from(rate) / x.len() as f64;
    let (k_lo, k_hi) = (
        ((lo / bin_hz).ceil() as usize).max(1),
        ((hi / bin_hz).floor() as usize).min(spec.len().saturating_sub(1)),
    );
    let mut num = 0.0f64;
    let mut den = 0.0f64;
    for (k, b) in spec.iter().enumerate().take(k_hi + 1).skip(k_lo) {
        let p = b.norm_sqr();
        num += p * (k as f64 * bin_hz);
        den += p;
    }
    if den > 1e-30 {
        num / den
    } else {
        0.5 * (lo + hi)
    }
}

/// Energy-weighted centroid of the harmonic peaks at `f0, 2·f0, …`
/// inside `[lo, hi]` Hz — an envelope-position metric immune to
/// harmonic-spacing changes (unlike plain band centroids, which shift
/// when a pitch change re-grids the harmonics inside the band).
#[must_use]
pub fn harmonic_centroid(x: &[f32], rate: u32, f0: f64, lo: f64, hi: f64) -> f64 {
    if x.is_empty() || f0 <= 0.0 {
        return 0.5 * (lo + hi);
    }
    let mut planner = RealFftPlanner::new();
    let r2c = planner.plan_fft_forward(x.len());
    let mut input: Vec<f64> = x.iter().map(|v| f64::from(*v)).collect();
    let mut spec = r2c.make_output_vec();
    r2c.process(&mut input, &mut spec)
        .expect("lengths pre-allocated");
    let bin_hz = f64::from(rate) / x.len() as f64;
    let mut num = 0.0f64;
    let mut den = 0.0f64;
    let mut h = 1usize;
    while f0 * h as f64 <= hi {
        let f = f0 * h as f64;
        if f >= lo {
            // Peak magnitude within +-1 bin of the harmonic.
            let k = (f / bin_hz).round() as usize;
            let hi = (k + 1).min(spec.len() - 1);
            let mut m = 0.0f64;
            for b in spec[k.saturating_sub(1)..=hi].iter() {
                m = m.max(b.norm_sqr());
            }
            num += m * f;
            den += m;
        }
        h += 1;
    }
    if den > 1e-30 {
        num / den
    } else {
        0.5 * (lo + hi)
    }
}

/// Energy-weighted centroid of 1/3-octave band levels within `[lo, hi]`
/// Hz — a grid-independent envelope-position metric. Plain band and
/// harmonic centroids are biased when a pitch shift re-grids the
/// harmonic comb; third-octave band energies are not.
#[must_use]
pub fn third_octave_centroid(x: &[f32], rate: u32, lo: f64, hi: f64) -> f64 {
    if x.is_empty() {
        return 0.5 * (lo + hi);
    }
    let mut planner = RealFftPlanner::new();
    let r2c = planner.plan_fft_forward(x.len());
    let mut input: Vec<f64> = x.iter().map(|v| f64::from(*v)).collect();
    let mut spec = r2c.make_output_vec();
    r2c.process(&mut input, &mut spec)
        .expect("lengths pre-allocated");
    let bin_hz = f64::from(rate) / x.len() as f64;
    let pow: Vec<f64> = spec.iter().map(|b| b.norm_sqr()).collect();
    // 1/3-octave bands, 10 per decade, standard centres.
    let mut num = 0.0f64;
    let mut den = 0.0f64;
    let mut f = 100.0f64;
    while f < f64::from(rate) / 2.0 {
        let (blo, bhi) = (f / 2f64.powf(1.0 / 6.0), f * 2f64.powf(1.0 / 6.0));
        if bhi >= lo && blo <= hi {
            let k_lo = ((blo / bin_hz).ceil() as usize).max(1);
            let k_hi = ((bhi / bin_hz).floor() as usize).min(pow.len() - 1);
            if k_hi >= k_lo {
                let e: f64 = pow[k_lo..=k_hi].iter().sum();
                // Fractional overlap with the measurement window.
                let overlap = (bhi.min(hi) - blo.max(lo)) / (bhi - blo);
                num += e * f * overlap;
                den += e * overlap;
            }
        }
        f *= 2f64.powf(1.0 / 3.0);
    }
    if den > 1e-30 {
        num / den
    } else {
        0.5 * (lo + hi)
    }
}
