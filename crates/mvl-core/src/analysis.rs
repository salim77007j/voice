//! Shared analysis layer (architecture plan §6.1).
//!
//! One pass per STFT frame produces everything the three processors
//! need: YIN F0 + clarity, RMS/level, spectral centroid, spectral
//! flatness, 4–10 kHz energy ratio, an onset-transient flag, and the
//! `Voiced / Sibilant / Breath / Silence` classification. The engine
//! feeds every processor from this single analysis so the treatments
//! stay mutually consistent.
//!
//! YIN runs on a 21 ms window (1024 samples at 48 kHz, scaled at other
//! rates) with an automatic 2× retry for low-F0 (male) voices. At
//! rates ≥ 96 kHz the detector runs on a stride-decimated copy of the
//! history — F0 ≤ 1 kHz needs at most ~48 kHz of bandwidth, and this
//! keeps analysis cost independent of the session rate.

use std::collections::VecDeque;

use pitch_detection::detector::yin::YINDetector;
use pitch_detection::detector::PitchDetector;
use rustfft::num_complex::Complex64;

/// Lowest F0 the engine claims to track.
pub const MIN_F0_HZ: f64 = 60.0;
/// Highest F0 the engine claims to track.
pub const MAX_F0_HZ: f64 = 1000.0;
/// Below this dBFS (frame RMS) a frame is silence.
pub const SILENCE_DBFS: f64 = -60.0;
/// YIN clarity above which a frame with a valid F0 is voiced.
pub const VOICED_CLARITY: f32 = 0.55;
/// 4–10 kHz energy ratio above which an aperiodic frame is sibilant.
pub const SIBILANT_HB_RATIO: f32 = 0.45;
/// Onset rise (dB between ~1.3 ms sub-windows) that flags a transient.
pub const TRANSIENT_RISE_DB: f64 = 15.0;

/// YIN power gate (sum of squares); catches near-digital-silence early.
const YIN_POWER_THRESHOLD: f64 = 1e-4;
/// YIN dip strictness passed to `pitch-detection` (1 − dip threshold);
/// 0.85 ⇒ the classic YIN absolute threshold of 0.15, which avoids
/// formant-beat octave errors on vowels.
const YIN_STRICTNESS: f64 = 0.85;
/// Below this base-window F0 the low-F0 (2× window) retry runs.
const LOW_F0_RETRY_HZ: f64 = 120.0;

/// Per-frame classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameClass {
    /// Periodic, pitched phonation.
    Voiced,
    /// Aperiodic, high-band dominant (/s/, /ʃ/, /tʃ/).
    Sibilant,
    /// Aperiodic, broadband (breaths, aspiration, room tone).
    Breath,
    /// Below the level floor.
    Silence,
}

/// Everything the processors know about one frame.
#[derive(Debug, Clone, Copy)]
pub struct FrameFeatures {
    /// YIN F0 when present and inside `[MIN_F0_HZ, MAX_F0_HZ]`.
    pub f0_hz: Option<f32>,
    /// YIN clarity `0..1` (0 when no estimate).
    pub clarity: f32,
    /// Frame RMS (linear).
    pub rms: f32,
    /// Frame RMS in dBFS.
    pub dbfs: f32,
    /// Spectral centroid in Hz.
    pub centroid_hz: f32,
    /// Spectral flatness `0..1` (1 = white).
    pub flatness: f32,
    /// 4–10 kHz energy / total energy.
    pub high_band_ratio: f32,
    /// True when a sharp energy rise (consonant attack) precedes this frame.
    pub transient: bool,
    /// Raw classification (before smoothing).
    pub class: FrameClass,
}

/// One-shot YIN estimate over a complete buffer (offline convenience;
/// the streaming analyser reuses its buffers instead).
///
/// Returns `(f0 Hz, voicing clarity 0..1)`. The clarity is the normalised
/// autocorrelation at the detected period — the upstream crate's own
/// `clarity` field has a broken scale (negative for pure tones), so we
/// measure periodicity ourselves (standard MPM-style normalisation).
/// The caller is responsible for window length: YIN needs ≥ 2 periods,
/// i.e. ≥ `2·rate/MIN_F0_HZ` samples for low voices.
#[must_use]
pub fn yin_f0(samples: &[f32], rate: u32) -> Option<(f64, f64)> {
    if samples.len() < 64 {
        return None;
    }
    let sig: Vec<f64> = samples.iter().map(|s| f64::from(*s)).collect();
    let mut det = YINDetector::<f64>::new(sig.len(), 0);
    let f0 = det
        .get_pitch(&sig, rate as usize, YIN_POWER_THRESHOLD, YIN_STRICTNESS)
        .map(|p| p.frequency);
    f0.map(|f| (f, autocorr_clarity(&sig, f, rate as usize)))
}

/// Normalised autocorrelation at the period of `f0`: ~1 for exactly
/// periodic content, ~0 for noise. Well-defined for any mixture in
/// between, which is what the voiced/breath boundary needs.
#[must_use]
fn autocorr_clarity(sig: &[f64], f0: f64, rate: usize) -> f64 {
    let lag = (rate as f64 / f0).round().max(2.0) as usize;
    if lag == 0 || lag >= sig.len() / 2 || !f0.is_finite() || f0 <= 0.0 {
        return 0.0;
    }
    let n = sig.len() - lag;
    let mut num = 0.0f64;
    let mut den = 0.0f64;
    for i in 0..n {
        num += sig[i] * sig[i + lag];
        den += sig[i] * sig[i];
    }
    (num / den.max(1e-30)).clamp(0.0, 1.0)
}

/// Streaming frame analyser.
///
/// Push raw samples with [`FrameAnalyzer::push_history`] (any chunking);
/// whenever the STFT produces a frame, call [`FrameAnalyzer::analyze`]
/// with the frame's end position (in real samples from the stream start)
/// and its spectrum.
pub struct FrameAnalyzer {
    rate: u32,
    /// Stride for high-rate decimation (1 below 96 kHz).
    decim: usize,
    base: usize,
    large: usize,
    det_base: YINDetector<f64>,
    det_large: YINDetector<f64>,
    hist: VecDeque<f64>,
    hist_start: usize,
    sub_len: usize,
    sub_sq: f64,
    sub_n: usize,
    sub_hist: VecDeque<f64>,
}

impl FrameAnalyzer {
    /// Create for a session sample rate.
    #[must_use]
    pub fn new(rate: u32) -> Self {
        let decim = if rate >= 96_000 && rate % 48_000 == 0 {
            (rate / 48_000) as usize
        } else {
            1
        };
        let base = 1024;
        let large = 2048;
        Self {
            rate,
            decim,
            base,
            large,
            det_base: YINDetector::new(base, 0),
            det_large: YINDetector::new(large, 0),
            hist: VecDeque::new(),
            hist_start: 0,
            sub_len: ((f64::from(rate) * 64.0 / 48_000.0).round() as usize).max(16),
            sub_sq: 0.0,
            sub_n: 0,
            sub_hist: VecDeque::new(),
        }
    }

    /// Session sample rate.
    #[must_use]
    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Feed raw samples (call before analysing frames that end inside
    /// this chunk; any chunking is fine).
    pub fn push_history(&mut self, samples: &[f32]) {
        for &s in samples {
            let v = f64::from(s);
            self.hist.push_back(v);
            self.sub_sq += v * v;
            self.sub_n += 1;
            if self.sub_n == self.sub_len {
                let rms = (self.sub_sq / self.sub_len as f64).sqrt();
                self.sub_hist.push_back(rms);
                if self.sub_hist.len() > 32 {
                    self.sub_hist.pop_front();
                }
                self.sub_sq = 0.0;
                self.sub_n = 0;
            }
        }
        // Keep enough raw history for the large YIN window plus slack.
        let keep = self.large * self.decim + self.sub_len * 32 + 4096;
        if self.hist.len() > keep {
            let drop = self.hist.len() - keep;
            self.hist.drain(..drop);
            self.hist_start += drop;
        }
    }

    /// Analyse the frame whose time-domain window ends at real-sample
    /// position `frame_end`, with the STFT frame's spectrum.
    #[must_use]
    pub fn analyze(
        &mut self,
        frame_end: usize,
        spectrum: &[Complex64],
        fft_size: usize,
    ) -> FrameFeatures {
        let (f0, clarity) = self.estimate_f0(frame_end);
        let in_range = f0.is_some_and(|f| (MIN_F0_HZ..=MAX_F0_HZ).contains(&f));
        let rms = self.window_rms(frame_end);
        let dbfs = (20.0 * (rms + 1e-12).log10()) as f32;
        let (centroid, flatness, hbr) = spectral_features(spectrum, fft_size, self.rate);
        let transient = self.detect_transient();
        let raw = classify(dbfs, clarity as f32, hbr as f32, in_range);
        // Frames whose analysis window is truncated by the stream start
        // (left-flush pad) cannot be judged for noise character — a
        // voiced onset would otherwise read as low-clarity "breath" and
        // get ducked. Keep a detected pitch (Voiced), else fall back to
        // Silence (no processing) for the first ~20 ms.
        let partial_window = frame_end < self.base * self.decim;
        let class = if partial_window && raw != FrameClass::Voiced {
            FrameClass::Silence
        } else {
            raw
        };
        FrameFeatures {
            f0_hz: if in_range { f0.map(|v| v as f32) } else { None },
            clarity: clarity as f32,
            rms: rms as f32,
            dbfs,
            centroid_hz: centroid as f32,
            flatness: flatness as f32,
            high_band_ratio: hbr as f32,
            transient,
            class,
        }
    }

    fn yin_rate(&self) -> usize {
        self.rate as usize / self.decim
    }

    /// Strided, left-zero-padded window of `win` decimated samples
    /// ending at `frame_end`.
    fn strided_window(&mut self, frame_end: usize, win: usize) -> Vec<f64> {
        let span = win * self.decim;
        let start = frame_end.saturating_sub(span);
        let mut sig = vec![0.0f64; win];
        for (i, s) in sig.iter_mut().enumerate() {
            let pos = start + i * self.decim;
            if pos >= self.hist_start {
                if let Some(v) = self.hist.get(pos - self.hist_start) {
                    *s = *v;
                }
            }
        }
        sig
    }

    fn estimate_f0(&mut self, frame_end: usize) -> (Option<f64>, f64) {
        let base_sig = self.strided_window(frame_end, self.base);
        let base_f0 = self
            .det_base
            .get_pitch(
                &base_sig,
                self.yin_rate(),
                YIN_POWER_THRESHOLD,
                YIN_STRICTNESS,
            )
            .map(|p| p.frequency);
        let needs_retry = base_f0.map_or(true, |f| f < LOW_F0_RETRY_HZ);
        let (f0, sig) = if needs_retry {
            let large_sig = self.strided_window(frame_end, self.large);
            let large_f0 = self
                .det_large
                .get_pitch(
                    &large_sig,
                    self.yin_rate(),
                    YIN_POWER_THRESHOLD,
                    YIN_STRICTNESS,
                )
                .map(|p| p.frequency);
            match (base_f0, large_f0) {
                (_, Some(lf)) if (MIN_F0_HZ..=MAX_F0_HZ).contains(&lf) => (Some(lf), large_sig),
                (Some(bf), _) => (Some(bf), base_sig),
                (None, None) => (None, base_sig),
                (None, Some(lf)) => (Some(lf), large_sig),
            }
        } else {
            (base_f0, base_sig)
        };
        match f0 {
            Some(f) => (Some(f), autocorr_clarity(&sig, f, self.yin_rate())),
            None => (None, 0.0),
        }
    }

    fn window_rms(&self, frame_end: usize) -> f64 {
        let span = self.base * self.decim;
        let start = frame_end.saturating_sub(span);
        let mut sum = 0.0f64;
        let mut n = 0usize;
        for pos in start..frame_end {
            if pos >= self.hist_start {
                if let Some(v) = self.hist.get(pos - self.hist_start) {
                    sum += v * v;
                    n += 1;
                }
            }
        }
        if n == 0 {
            0.0
        } else {
            (sum / n as f64).sqrt()
        }
    }

    fn detect_transient(&self) -> bool {
        if self.sub_hist.len() < 2 {
            return false;
        }
        let recent: Vec<f64> = self.sub_hist.iter().rev().take(6).copied().collect();
        // `recent` is newest-first; compare each adjacent (older, newer) pair.
        for pair in recent.windows(2) {
            let (older, newer) = (pair[1], pair[0]);
            let d_old = 20.0 * (older + 1e-12).log10();
            let d_new = 20.0 * (newer + 1e-12).log10();
            if d_new - d_old > TRANSIENT_RISE_DB && d_new > SILENCE_DBFS + 6.0 {
                return true;
            }
        }
        false
    }
}

/// Pure classification rule (unit-tested on its own).
#[must_use]
pub fn classify(dbfs: f32, clarity: f32, high_band_ratio: f32, f0_valid: bool) -> FrameClass {
    if dbfs < SILENCE_DBFS as f32 {
        return FrameClass::Silence;
    }
    if f0_valid && clarity >= VOICED_CLARITY {
        return FrameClass::Voiced;
    }
    if high_band_ratio >= SIBILANT_HB_RATIO {
        return FrameClass::Sibilant;
    }
    FrameClass::Breath
}

/// Centroid (Hz), flatness `0..1` and 4–10 kHz energy ratio of a
/// windowed complex spectrum.
#[must_use]
pub fn spectral_features(spectrum: &[Complex64], fft_size: usize, rate: u32) -> (f64, f64, f64) {
    let bin_hz = f64::from(rate) / fft_size as f64;
    let mut total = 0.0f64;
    let mut weighted = 0.0f64;
    let mut log_sum = 0.0f64;
    let mut hb = 0.0f64;
    let mut n = 0usize;
    for (k, b) in spectrum.iter().enumerate().skip(1) {
        let p = b.norm_sqr();
        let f = k as f64 * bin_hz;
        total += p;
        weighted += p * f;
        log_sum += (p + 1e-40).ln();
        if (4000.0..=10_000.0).contains(&f) {
            hb += p;
        }
        n += 1;
    }
    let centroid = if total > 1e-30 { weighted / total } else { 0.0 };
    let flatness = if n > 0 {
        let geo = (log_sum / n as f64).exp();
        let arith = (total + 1e-40) / n as f64;
        (geo / arith).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let hbr = if total > 1e-30 { hb / total } else { 0.0 };
    (centroid, flatness, hbr)
}

/// Causal majority-vote class smoother (the "hysteresis" of plan §6.1).
///
/// Depth 1 is a passthrough; larger depths require several consecutive
/// raw flips before the smoothed class changes, which kills single-frame
/// flicker at segment boundaries. The engine uses depth 3 for preview
/// and 7 for render.
#[derive(Debug)]
pub struct ClassSmoother {
    window: VecDeque<FrameClass>,
    depth: usize,
}

impl ClassSmoother {
    /// Create with smoothing depth (≥ 1).
    #[must_use]
    pub fn new(depth: usize) -> Self {
        Self {
            window: VecDeque::new(),
            depth: depth.max(1),
        }
    }

    /// Feed one raw class; returns the smoothed class.
    pub fn push(&mut self, raw: FrameClass) -> FrameClass {
        self.window.push_back(raw);
        if self.window.len() > self.depth {
            self.window.pop_front();
        }
        mode(self.window.iter()).unwrap_or(raw)
    }

    /// Reset state.
    pub fn reset(&mut self) {
        self.window.clear();
    }
}

fn mode<'a, I>(classes: I) -> Option<FrameClass>
where
    I: IntoIterator<Item = &'a FrameClass>,
{
    let classes: Vec<&FrameClass> = classes.into_iter().collect();
    let mut counts = [0usize; 4];
    for c in &classes {
        counts[**c as usize] += 1;
    }
    let best = counts.iter().max()?;
    if *best * 2 > classes.len() {
        counts.iter().position(|c| c == best).map(|i| match i {
            0 => FrameClass::Voiced,
            1 => FrameClass::Sibilant,
            2 => FrameClass::Breath,
            _ => FrameClass::Silence,
        })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stft::StftAnalyzer;
    use crate::testsupport as ts;

    const RATE: u32 = 48_000;
    const N: usize = 512;
    const HOP: usize = 128;

    /// Realistic streaming mini-pipeline: history and STFT are pushed
    /// chunk by chunk exactly the way the engine does.
    fn analyze_streaming(sig: &[f32]) -> Vec<FrameFeatures> {
        let mut planner = realfft::RealFftPlanner::new();
        let mut stft = StftAnalyzer::new(&mut planner, N, HOP);
        let mut an = FrameAnalyzer::new(RATE);
        let mut feats = Vec::new();
        let mut pos = 0;
        while pos < sig.len() {
            let end = (pos + HOP).min(sig.len());
            an.push_history(&sig[pos..end]);
            for f in stft.push(&sig[pos..end]) {
                feats.push(an.analyze(f.start + HOP, &f.spectrum, N));
            }
            pos = end;
        }
        for f in stft.flush() {
            feats.push(an.analyze(f.start + HOP, &f.spectrum, N));
        }
        feats
    }

    /// Middle 60 % of frames — skips stream edges.
    fn middle(feats: &[FrameFeatures]) -> &[FrameFeatures] {
        let n = feats.len();
        &feats[n / 5..n - n / 5]
    }

    #[test]
    fn voiced_sine_is_detected() {
        let sig = ts::sine(220.0, 0.5, RATE as usize, RATE);
        let feats = analyze_streaming(&sig);
        let mid = middle(&feats);
        assert!(!mid.is_empty());
        let voiced = mid.iter().filter(|f| f.class == FrameClass::Voiced).count();
        assert!(
            voiced > mid.len() * 9 / 10,
            "only {voiced}/{} middle frames voiced",
            mid.len()
        );
        let avg_f0 = mid.iter().filter_map(|f| f.f0_hz).sum::<f32>()
            / mid.iter().filter(|f| f.f0_hz.is_some()).count() as f32;
        assert!((avg_f0 - 220.0).abs() < 2.0, "f0 drifted: {avg_f0}");
        assert!(mid.iter().all(|f| f.clarity > 0.8), "clarity too low");
    }

    #[test]
    fn male_low_f0_uses_large_window_retry() {
        let sig = ts::harmonic_stack(85.0, 24, 0.5, RATE as usize, RATE);
        let feats = analyze_streaming(&sig);
        let mid = middle(&feats);
        let voiced = mid.iter().filter(|f| f.class == FrameClass::Voiced).count();
        assert!(
            voiced > mid.len() * 9 / 10,
            "only {voiced}/{} middle frames voiced",
            mid.len()
        );
        let f0s: Vec<f32> = mid.iter().filter_map(|f| f.f0_hz).collect();
        let avg = f0s.iter().sum::<f32>() / f0s.len() as f32;
        assert!((avg - 85.0).abs() < 2.0, "male f0 drifted: {avg}");
    }

    #[test]
    fn sibilant_noise_is_classified() {
        let sig = ts::sibilant_noise(7, RATE as usize, RATE, 0.1);
        let feats = analyze_streaming(&sig);
        let mid = middle(&feats);
        let sib = mid
            .iter()
            .filter(|f| f.class == FrameClass::Sibilant)
            .count();
        assert!(
            sib > mid.len() * 9 / 10,
            "only {sib}/{} middle frames sibilant",
            mid.len()
        );
        assert!(mid.iter().all(|f| f.high_band_ratio > 0.8));
    }

    #[test]
    fn breath_noise_is_classified() {
        let sig = ts::breath_noise(11, RATE as usize, RATE, 0.063);
        let feats = analyze_streaming(&sig);
        let mid = middle(&feats);
        let breath = mid.iter().filter(|f| f.class == FrameClass::Breath).count();
        assert!(
            breath > mid.len() * 9 / 10,
            "only {breath}/{} middle frames breath",
            mid.len()
        );
        assert!(mid.iter().all(|f| f.high_band_ratio < 0.45));
    }

    #[test]
    fn silence_is_classified() {
        let sig = ts::silence(RATE as usize);
        let feats = analyze_streaming(&sig);
        assert!(feats.iter().all(|f| f.class == FrameClass::Silence));
    }

    #[test]
    fn transient_flag_fires_at_onset() {
        // 200 ms silence, then a 2 ms attack into a steady tone.
        let onset = 9_600;
        let sig = ts::concat(
            &ts::silence(onset),
            &ts::fade_in(ts::sine(180.0, 0.6, 24_000, RATE), 2.0, RATE),
        );
        let feats = analyze_streaming(&sig);
        let mut fired = false;
        for (i, f) in feats.iter().enumerate() {
            let t = i * HOP;
            if (onset..onset + 12_000).contains(&t) && f.transient {
                fired = true;
            }
            // Steady state (well past the attack) must not be transient.
            if t > onset + 20_000 {
                assert!(!f.transient, "transient stuck on at {t}");
            }
        }
        assert!(fired, "no transient flagged around the onset");
    }

    #[test]
    fn abrupt_voiced_start_is_not_misclassified_as_breath() {
        // Regression: left-flush pad frames of an abruptly starting
        // voiced signal must never classify as Breath/Sibilant.
        let sig = ts::harmonic_stack(196.0, 16, 0.5, RATE as usize, RATE);
        let feats = analyze_streaming(&sig);
        assert!(
            feats
                .iter()
                .all(|f| f.class == FrameClass::Voiced || f.class == FrameClass::Silence),
            "edge frames misclassified: {:?}",
            feats.iter().map(|f| f.class).collect::<Vec<_>>()
        );
        // And from the moment the window is fully inside the signal,
        // everything is voiced.
        let settled = &feats[16..];
        assert!(settled.iter().all(|f| f.class == FrameClass::Voiced));
    }

    #[test]
    fn classify_rule_edges() {
        use FrameClass::*;
        assert_eq!(classify(-61.0, 0.9, 0.0, true), Silence);
        assert_eq!(classify(-30.0, 0.9, 0.0, true), Voiced);
        assert_eq!(classify(-30.0, 0.9, 0.0, false), Breath, "no valid f0");
        assert_eq!(classify(-30.0, 0.3, 0.9, false), Sibilant);
        assert_eq!(classify(-30.0, 0.3, 0.1, false), Breath);
        // Voiced wins over sibilant when periodic.
        assert_eq!(classify(-30.0, 0.8, 0.9, true), Voiced);
    }

    #[test]
    fn smoother_kills_single_frame_flicker() {
        use FrameClass::*;
        let mut sm = ClassSmoother::new(3);
        let out: Vec<FrameClass> = [Voiced, Voiced, Breath, Voiced, Voiced]
            .iter()
            .map(|c| sm.push(*c))
            .collect();
        assert!(out.iter().all(|c| *c == Voiced), "{out:?}");
    }

    #[test]
    fn smoother_depth_one_is_passthrough() {
        use FrameClass::*;
        let mut sm = ClassSmoother::new(1);
        assert_eq!(sm.push(Voiced), Voiced);
        assert_eq!(sm.push(Breath), Breath);
    }

    #[test]
    fn spectral_features_single_bin() {
        // All energy in bin k=100 of a 512-pt FFT at 48 kHz = 9375 Hz.
        let mut spec = vec![Complex64::new(0.0, 0.0); N / 2 + 1];
        spec[100] = Complex64::new(10.0, 0.0);
        let (centroid, flatness, hbr) = spectral_features(&spec, N, RATE);
        assert!((centroid - 9375.0).abs() < 1.0, "centroid {centroid}");
        assert!(flatness < 1e-5, "flatness {flatness}");
        assert!((hbr - 1.0).abs() < 1e-9, "hbr {hbr}");
        // Energy only at 1 kHz: no high-band content.
        let mut spec2 = vec![Complex64::new(0.0, 0.0); N / 2 + 1];
        spec2[10000 / (RATE as usize / N)] = Complex64::new(10.0, 0.0);
        let (_, _, hbr2) = spectral_features(&spec2, N, RATE);
        assert!(hbr2 < 1e-9, "hbr2 {hbr2}");
    }

    #[test]
    fn yin_f0_helper_matches_streaming() {
        let sig = ts::harmonic_stack(196.0, 16, 0.5, 4096, RATE);
        let (f0, clarity) = yin_f0(&sig, RATE).expect("no pitch");
        assert!((f0 - 196.0).abs() < 2.0, "f0 {f0}");
        assert!(clarity > 0.8);
    }
}
