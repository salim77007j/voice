//! Pitch shifter — ±12 semitones at 1-cent resolution, timing preserved
//! (architecture plan §6.2).
//!
//! Chain: **phase-locked phase-vocoder TSM → windowed-sinc ratio
//! conversion**. The vocoder time-stretches by `r = 2^(semitones/12)`
//! while keeping pitch and timbre (identity phase locking after
//! Laroche & Dolson), then the ratio converter reads the stretched
//! signal `r`× faster — pitch moves by `r`, duration is restored.
//!
//! The anti-chipmunk formant compensation is *not* here: it is applied
//! by the formant stage of the engine, which receives `g_user / r` so
//! the net formant displacement equals the user's mm slider (plan
//! §6.5). This module only moves pitch.
//!
//! Timing exactness: synthesis frames land at absolutely computed
//! positions `q_m = round(m·hop·r)` (integer Bresenham-style
//! scheduling — zero cumulative drift), the OLA denominator absorbs the
//! ±1-sample hop jitter exactly, and the ratio converter's group delay
//! is discarded at the stream start. The engine pads/trims the final
//! sample to whole-stream equality.

use std::f64::consts::TAU;
use std::sync::Arc;

use realfft::{ComplexToReal, RealFftPlanner};
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::audioadapter_buffers::owned::InterleavedOwned;
use rubato::{
    Async, FixedAsync, Indexing, Resampler, SincInterpolationParameters, SincInterpolationType,
    WindowFunction,
};
use rustfft::num_complex::Complex64;

use crate::error::EngineError;
use crate::stft::{hann_window, OverlapAdder};

/// Wrap an angle to `(-π, π]`.
#[must_use]
pub fn princarg(x: f64) -> f64 {
    let wrapped = x - TAU * (x / TAU).round();
    if wrapped > std::f64::consts::PI {
        wrapped - TAU
    } else if wrapped <= -std::f64::consts::PI {
        wrapped + TAU
    } else {
        wrapped
    }
}

/// Phase-vocoder time-scale modification with identity phase locking.
///
/// Consumes *analysis* spectra (already windowed and transformed by the
/// engine's shared STFT — possibly gain-modified by the breath/formant
/// stages) and overlap-adds synthesis frames at stretched positions.
pub struct PhaseVocoder {
    n: usize,
    hop: usize,
    bins: usize,
    ratio: f64,
    c2r: Arc<dyn ComplexToReal<f64>>,
    ola: OverlapAdder,
    prev_phi: Vec<f64>,
    prev_psi: Vec<f64>,
    have_prev: bool,
    frame_index: usize,
    last_pos: usize,
}

impl PhaseVocoder {
    /// Create for a stretch factor `ratio` (output duration = input ×
    /// ratio). `fft_size`/`hop` must match the analysis STFT.
    ///
    /// # Errors
    /// [`EngineError::Internal`] if the inverse FFT cannot be planned
    /// (fixed sizes — practically unreachable).
    pub fn new(
        planner: &mut RealFftPlanner<f64>,
        fft_size: usize,
        hop: usize,
        ratio: f64,
    ) -> Result<Self, EngineError> {
        assert!(
            ratio.is_finite() && ratio > 0.0,
            "stretch ratio must be positive"
        );
        let bins = fft_size / 2 + 1;
        Ok(Self {
            n: fft_size,
            hop,
            bins,
            ratio,
            c2r: planner.plan_fft_inverse(fft_size),
            ola: OverlapAdder::new(fft_size, hann_window(fft_size)),
            prev_phi: vec![0.0; bins],
            prev_psi: vec![0.0; bins],
            have_prev: false,
            frame_index: 0,
            last_pos: 0,
        })
    }

    /// Absolute (TSM-time) position where frame `m` will be placed:
    /// `round(m · hop · ratio)` — absolute positions, so rounding never
    /// accumulates drift.
    fn frame_pos(&self, m: usize) -> usize {
        (m as f64 * self.hop as f64 * self.ratio).round() as usize
    }

    /// Feed one (possibly gain-modified) analysis spectrum. `voiced`
    /// selects full peak-region phase locking (voiced) versus reduced
    /// ±1-bin locking (unvoiced, preserving noise character).
    ///
    /// # Panics
    /// Panics if the spectrum length does not match the FFT size — an
    /// engine wiring bug, not user data.
    pub fn push_frame(&mut self, spectrum: &[Complex64], voiced: bool) {
        assert_eq!(spectrum.len(), self.bins, "spectrum/bin mismatch");
        let pos = self.frame_pos(self.frame_index);
        let hs = pos - self.last_pos;

        let mut phi: Vec<f64> = Vec::with_capacity(self.bins);
        let mut mag: Vec<f64> = Vec::with_capacity(self.bins);
        for b in spectrum {
            phi.push(b.arg());
            mag.push(b.norm());
        }

        let mut psi;
        if !self.have_prev {
            // First frame: identity phases.
            psi = phi.clone();
            self.have_prev = true;
        } else {
            // Heterodyned phase increment → instantaneous frequency.
            let mut target = vec![0.0f64; self.bins];
            for k in 0..self.bins {
                let expected =
                    phi[k] - self.prev_phi[k] - TAU * (k * self.hop) as f64 / self.n as f64;
                let dev = princarg(expected);
                let omega = TAU * k as f64 / self.n as f64 + dev / self.hop as f64;
                target[k] = self.prev_psi[k] + omega * hs as f64;
            }
            psi = target.clone();

            // --- Identity phase locking (Laroche & Dolson) ---
            let frame_max = mag.iter().copied().fold(0.0f64, f64::max);
            if frame_max > 1e-30 {
                let peak_floor = frame_max * 10.0f64.powf(-50.0 / 20.0);
                let peaks: Vec<usize> = (1..self.bins - 1)
                    .filter(|&k| mag[k] > mag[k - 1] && mag[k] >= mag[k + 1] && mag[k] > peak_floor)
                    .collect();
                if !peaks.is_empty() {
                    for (i, &p) in peaks.iter().enumerate() {
                        let (lo, hi) = if voiced {
                            let lo = if i == 0 {
                                1
                            } else {
                                (peaks[i - 1] + p).div_ceil(2)
                            };
                            let hi = if i + 1 == peaks.len() {
                                self.bins - 2
                            } else {
                                (p + peaks[i + 1]) / 2
                            };
                            (lo, hi)
                        } else {
                            // Reduced locking for noise-like frames.
                            (p.saturating_sub(1), (p + 1).min(self.bins - 2))
                        };
                        psi[p] = target[p];
                        for k in lo..=hi {
                            if k != p && k >= 1 && k < self.bins - 1 {
                                psi[k] = princarg(psi[p] + (phi[k] - phi[p]));
                            }
                        }
                    }
                }
            }
            // DC and Nyquist bypass phase propagation entirely.
            psi[0] = phi[0];
            psi[self.bins - 1] = phi[self.bins - 1];
            for p in &mut psi {
                *p = princarg(*p);
            }
        }

        // Resynthesise, inverse-transform (unnormalised realfft), OLA.
        let mut spec: Vec<Complex64> = psi
            .iter()
            .zip(&mag)
            .map(|(&p, &m)| Complex64::new(m * p.cos(), m * p.sin()))
            .collect();
        // realfft's inverse requires exact-zero imaginary parts at DC and
        // Nyquist (phase ±π survives in the real sign).
        spec[0].im = 0.0;
        spec[self.bins - 1].im = 0.0;
        let mut time = vec![0.0f64; self.n];
        self.c2r
            .process(&mut spec, &mut time)
            .expect("pre-sized buffers");
        let scale = 1.0 / self.n as f64;
        for v in &mut time {
            *v *= scale;
        }
        self.ola.add_frame_at(&time, pos);

        self.prev_phi = phi;
        self.prev_psi = psi;
        self.last_pos = pos;
        self.frame_index += 1;
    }

    /// Current OLA frontier (TSM-time samples below it are final).
    #[must_use]
    pub fn frontier(&self) -> usize {
        self.ola.frontier()
    }

    /// Pop finalised TSM samples below `limit`.
    #[must_use]
    pub fn pop_ready(&mut self, limit: usize) -> Vec<f64> {
        self.ola
            .pop_ready(limit)
            .into_iter()
            .map(f64::from)
            .collect()
    }

    /// End of stream: pop everything written so far (samples beyond the
    /// frontier are final too — no future frames will cover them).
    #[must_use]
    pub fn drain(&mut self) -> Vec<f64> {
        let limit = self.ola.frontier() + self.n;
        self.ola.drain(limit).into_iter().map(f64::from).collect()
    }
}

/// Streaming windowed-sinc ratio converter (rubato `Async` sinc, 32-tap
/// Blackman-Harris-2 window, cubic kernel interpolation).
///
/// Wraps the resampler with the two details that make it usable for
/// surgical timing: the group delay (`output_delay` frames) is
/// discarded at the stream start, and [`RatioConverter::flush`] pads
/// with silence until the full aligned output has been produced.
pub struct RatioConverter {
    resampler: Async<f64>,
    chunk_in: usize,
    pending: Vec<f64>,
    out: std::collections::VecDeque<f64>,
    skip: usize,
    skipped: usize,
    emitted: usize,
    produced: usize,
    total_in: usize,
    ratio: f64,
    flushed: bool,
}

impl RatioConverter {
    /// Create for `ratio` = output frames per input frame (e.g. `1/r`
    /// for the pitch path, where the TSM output is read `r`× faster).
    ///
    /// # Errors
    /// [`EngineError::Internal`] when rubato rejects the configuration.
    pub fn new(ratio: f64) -> Result<Self, EngineError> {
        let params = SincInterpolationParameters::new(32, WindowFunction::BlackmanHarris2)
            .oversampling_factor(128)
            .interpolation(SincInterpolationType::Cubic);
        let chunk_in = 256;
        let resampler = Async::<f64>::new_sinc(ratio, 2.0, &params, chunk_in, 1, FixedAsync::Input)
            .map_err(|e| EngineError::Internal(format!("sinc resampler: {e}")))?;
        let skip = resampler.output_delay();
        Ok(Self {
            resampler,
            chunk_in,
            pending: Vec::new(),
            out: std::collections::VecDeque::new(),
            skip,
            skipped: 0,
            emitted: 0,
            produced: 0,
            total_in: 0,
            ratio,
            flushed: false,
        })
    }

    /// Group delay discarded at the stream start, in output frames.
    #[must_use]
    pub fn output_delay(&self) -> usize {
        self.skip
    }

    /// Feed input samples (any amount).
    pub fn push(&mut self, samples: &[f64]) {
        assert!(!self.flushed, "push after flush");
        self.pending.extend_from_slice(samples);
        self.total_in += samples.len();
        while self.pending.len() >= self.chunk_in {
            self.process_full();
        }
    }

    /// Pop up to `max` aligned output frames.
    #[must_use]
    pub fn pop_available(&mut self, max: usize) -> Vec<f64> {
        self.discard_skip();
        let take = max.min(self.out.len());
        let mut v = Vec::with_capacity(take);
        for _ in 0..take {
            v.push(self.out.pop_front().unwrap_or(0.0));
        }
        self.emitted += take;
        v
    }

    /// End of stream: flush the partial input chunk, then feed silence
    /// until the aligned output is complete, and mark finished.
    pub fn flush(&mut self) {
        if self.flushed {
            return;
        }
        self.flushed = true;
        if !self.pending.is_empty() {
            let n = self.pending.len().min(self.chunk_in);
            let tail: Vec<f64> = self.pending.drain(..n).collect();
            self.process_indexed(&tail, Some(n));
            self.pending.clear();
        }
        // Target: `skip` discarded frames + the aligned stream length
        // (plus one chunk of margin), then stop feeding silence.
        let target =
            self.skip + (self.total_in as f64 * self.ratio).round() as usize + self.chunk_in;
        let mut guard = 0;
        while self.produced < target && guard < 64 {
            self.process_indexed(&[], Some(0));
            guard += 1;
        }
    }

    /// Everything still buffered (post-flush drain helper).
    #[must_use]
    pub fn pop_remaining(&mut self) -> Vec<f64> {
        self.discard_skip();
        let v: Vec<f64> = self.out.drain(..).collect();
        self.emitted += v.len();
        v
    }

    fn process_full(&mut self) {
        let chunk: Vec<f64> = self.pending.drain(..self.chunk_in).collect();
        self.process_indexed(&chunk, None);
    }

    fn process_indexed(&mut self, input: &[f64], partial: Option<usize>) {
        let out_next = self.resampler.output_frames_next();
        let mut out_buf = InterleavedOwned::new(0.0f64, 1, out_next);
        let indexing = Indexing {
            partial_len: partial,
            ..Indexing::default()
        };
        // `input` is exactly the frames the resampler may read (full
        // chunk, or the real tail with partial_len).
        let frames = input.len();
        let in_buf = InterleavedSlice::new(input, 1, frames).expect("mono adapter length matches");
        let (_used, made) = self
            .resampler
            .process_into_buffer(&in_buf, &mut out_buf, Some(&indexing))
            .unwrap_or_else(|e| panic!("sinc processing failed: {e}"));
        for s in &out_buf.take_data()[..made] {
            self.out.push_back(*s);
        }
        self.produced += made;
    }

    fn discard_skip(&mut self) {
        while self.skipped < self.skip {
            if self.out.is_empty() {
                return;
            }
            self.out.pop_front();
            self.skipped += 1;
        }
    }
}

/// The complete pitch path: phase-vocoder TSM × `r` followed by sinc
/// ratio conversion × `1/r`. Output is time-aligned with the input
/// (leading STFT pad removed, group delay removed) and within a couple
/// of samples of the input length; the engine enforces exact equality.
pub struct PitchPath {
    pv: PhaseVocoder,
    rc: RatioConverter,
    lead: usize,
    lead_remaining: usize,
    emitted: usize,
}

impl PitchPath {
    /// Create for pitch ratio `r` (`2^(semitones/12)`).
    ///
    /// # Errors
    /// Propagates [`EngineError::Internal`] from construction.
    pub fn new(
        planner: &mut RealFftPlanner<f64>,
        fft_size: usize,
        hop: usize,
        r: f64,
    ) -> Result<Self, EngineError> {
        let pv = PhaseVocoder::new(planner, fft_size, hop, r)?;
        let rc = RatioConverter::new(1.0 / r)?;
        Ok(Self {
            pv,
            rc,
            lead: fft_size - hop,
            lead_remaining: fft_size - hop,
            emitted: 0,
        })
    }

    /// Feed one (possibly gain-modified) analysis spectrum.
    pub fn push_frame(&mut self, spectrum: &[Complex64], voiced: bool) {
        self.pv.push_frame(spectrum, voiced);
        let ready = self.pv.pop_ready(self.pv.frontier());
        self.rc.push(&ready);
    }

    /// Pop up to `max` aligned, lead-trimmed output samples.
    #[must_use]
    pub fn pop_output(&mut self, max: usize) -> Vec<f32> {
        let want = max + self.lead_remaining;
        let raw = self.rc.pop_available(want);
        let out = self.trim_lead(raw, max);
        self.emitted += out.len();
        out
    }

    /// End of stream: drain the vocoder, flush the converter, and return
    /// the remaining output trimmed/padded to exactly `target_len`
    /// samples (the engine invariant: sample-count equality with the
    /// input). Overshoot comes from trailing frame coverage and the
    /// converter's safety margin; shortfall is zero-padded (rare, ≤ 1
    /// chunk).
    #[must_use]
    pub fn flush(&mut self, target_len: usize) -> Vec<f32> {
        let streamed = self.emitted;
        let tail = self.pv.drain();
        self.rc.push(&tail);
        self.rc.flush();
        let raw = self.rc.pop_remaining();
        let out = self.trim_lead(raw, usize::MAX);
        self.emitted += out.len();
        // The stream total must be exactly `target_len`: trim the
        // overshoot (trailing frame coverage + converter margin) or
        // zero-pad the rare shortfall.
        let remaining = target_len.saturating_sub(streamed);
        let mut result: Vec<f32> = out.into_iter().take(remaining).collect();
        result.resize(remaining, 0.0);
        result
    }

    fn trim_lead(&mut self, mut raw: Vec<f64>, max: usize) -> Vec<f32> {
        if self.lead_remaining > 0 {
            let drop = self.lead_remaining.min(raw.len());
            raw.drain(..drop);
            self.lead_remaining -= drop;
        }
        raw.truncate(max);
        raw.into_iter().map(|v| v as f32).collect()
    }

    /// Total group delay (vocoder pad + sinc delay), for engine
    /// latency reporting.
    #[must_use]
    pub fn total_delay(&self) -> usize {
        self.lead + self.rc.output_delay()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stft::StftAnalyzer;
    use crate::testsupport as ts;

    const RATE: u32 = 48_000;

    /// Drive the full pitch path over `sig` with the preview STFT shape.
    fn run_pitch(sig: &[f32], semitones: f64) -> Vec<f32> {
        let (n, hop) = (512, 128);
        let r = 2f64.powf(semitones / 12.0);
        let mut planner = RealFftPlanner::new();
        let mut stft = StftAnalyzer::new(&mut planner, n, hop);
        let mut path = PitchPath::new(&mut planner, n, hop, r).unwrap();
        let mut out: Vec<f32> = Vec::new();
        let mut pos = 0;
        while pos < sig.len() {
            let end = (pos + hop).min(sig.len());
            for f in stft.push(&sig[pos..end]) {
                path.push_frame(&f.spectrum, true);
            }
            out.extend(path.pop_output(hop * 4));
            pos = end;
        }
        for f in stft.flush() {
            path.push_frame(&f.spectrum, true);
        }
        out.extend(path.flush(sig.len()));
        out
    }

    #[test]
    fn princarg_wraps_to_pm_pi() {
        assert!((princarg(0.0)).abs() < 1e-12);
        assert!((princarg(std::f64::consts::PI) - std::f64::consts::PI).abs() < 1e-12);
        assert!((princarg(-std::f64::consts::PI) - std::f64::consts::PI).abs() < 1e-12);
        assert!((princarg(3.0 * std::f64::consts::PI) - std::f64::consts::PI).abs() < 1e-9);
        assert!((princarg(0.5) - 0.5).abs() < 1e-12);
    }

    #[test]
    fn ratio_converter_impulse_stays_aligned() {
        // Impulse at input 1000, down-conversion by 0.5: the aligned
        // output must show the impulse at 500 (+-2 frames).
        let mut rc = RatioConverter::new(0.5).unwrap();
        let mut input = vec![0.0f64; 4096];
        input[1000] = 1.0;
        let mut out = Vec::new();
        for chunk in input.chunks(777) {
            rc.push(chunk);
            out.extend(rc.pop_available(4096));
        }
        rc.flush();
        out.extend(rc.pop_remaining());
        let peak = out
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.abs().partial_cmp(&b.1.abs()).unwrap())
            .map(|(i, v)| (i, *v))
            .expect("non-empty");
        assert!(
            (peak.0 as f64 - 500.0).abs() <= 2.0,
            "impulse landed at {} (expected 500), value {}",
            peak.0,
            peak.1
        );
    }

    #[test]
    fn ratio_converter_moves_tone_by_inverse_ratio() {
        // 440 Hz read 1.5x faster = 660 Hz.
        let mut rc = RatioConverter::new(1.0 / 1.5).unwrap();
        let input: Vec<f64> = ts::sine(440.0, 0.8, 24_000, RATE)
            .into_iter()
            .map(f64::from)
            .collect();
        let mut out = Vec::new();
        for chunk in input.chunks(512) {
            rc.push(chunk);
            out.extend(rc.pop_available(8192));
        }
        rc.flush();
        out.extend(rc.pop_remaining());
        let out_f32: Vec<f32> = out.into_iter().map(|v| v as f32).collect();
        let f = ts::band_peak_freq(&out_f32[500..], RATE, 500.0, 900.0).expect("peak");
        assert!(
            (f - 660.0).abs() < 5.0,
            "tone moved to {f} Hz, expected 660"
        );
    }

    #[test]
    fn tsm_preserves_pitch_and_stretches_duration() {
        // Vocoder alone (ratio 1.5): 220 Hz stays 220 Hz, duration ~1.5x.
        let (n, hop) = (512, 128);
        let sig = ts::sine(220.0, 0.5, 24_000, RATE);
        let mut planner = RealFftPlanner::new();
        let mut stft = StftAnalyzer::new(&mut planner, n, hop);
        let mut pv = PhaseVocoder::new(&mut planner, n, hop, 1.5).unwrap();
        let mut tsm: Vec<f64> = Vec::new();
        for f in stft.push(&sig) {
            pv.push_frame(&f.spectrum, true);
            tsm.extend(pv.pop_ready(pv.frontier()));
        }
        for f in stft.flush() {
            pv.push_frame(&f.spectrum, true);
        }
        tsm.extend(pv.drain());
        let expected = 24_000.0f64 * 1.5;
        assert!(
            (tsm.len() as f64 - expected).abs() < expected * 0.05,
            "TSM length {} vs expected ~{expected}",
            tsm.len()
        );
        let tsm_f32: Vec<f32> = tsm.iter().map(|v| *v as f32).collect();
        let f0 = ts::band_peak_freq(&tsm_f32[2000..tsm_f32.len() - 2000], RATE, 150.0, 300.0)
            .expect("peak");
        assert!((f0 - 220.0).abs() < 3.0, "TSM changed pitch: {f0} Hz");
    }

    #[test]
    fn pitch_path_shifts_f0_up_and_keeps_duration() {
        let sig = ts::harmonic_stack(220.0, 12, 0.5, 24_000, RATE);
        let out = run_pitch(&sig, 7.0);
        assert!(
            out.len() == sig.len(),
            "duration {} vs {}",
            out.len(),
            sig.len()
        );
        let core = &out[1000..out.len() - 1000];
        let f = ts::band_peak_freq(core, RATE, 300.0, 420.0).expect("peak");
        let expected = 220.0 * 2f64.powf(7.0 / 12.0);
        assert!(
            (f - expected).abs() < expected * 0.005,
            "shifted to {f} Hz, expected {expected:.2}"
        );
        assert!(core.iter().all(|v| v.is_finite()));
    }

    #[test]
    fn pitch_path_shifts_f0_down() {
        let sig = ts::harmonic_stack(300.0, 12, 0.5, 24_000, RATE);
        let out = run_pitch(&sig, -5.0);
        assert!(
            out.len() == sig.len(),
            "duration {} vs {}",
            out.len(),
            sig.len()
        );
        let core = &out[1000..out.len() - 1000];
        let f = ts::band_peak_freq(core, RATE, 180.0, 280.0).expect("peak");
        let expected = 300.0 * 2f64.powf(-5.0 / 12.0);
        assert!(
            (f - expected).abs() < expected * 0.008,
            "shifted to {f} Hz, expected {expected:.2}"
        );
    }

    #[test]
    fn pitch_path_survives_extreme_octaves() {
        for st in [12.0f64, -12.0] {
            let sig = ts::harmonic_stack(200.0, 10, 0.5, 24_000, RATE);
            let out = run_pitch(&sig, st);
            assert!(
                (out.len() as i64 - sig.len() as i64).abs() <= 4,
                "st={st}: duration {} vs {}",
                out.len(),
                sig.len()
            );
            let core = &out[1500..out.len() - 1500];
            let expected = 200.0 * 2f64.powf(st / 12.0);
            let lo = expected * 0.97;
            let hi = expected * 1.03;
            let f = ts::band_peak_freq(core, RATE, lo, hi)
                .unwrap_or_else(|| panic!("st={st}: no peak in [{lo},{hi}]"));
            assert!(
                (f - expected).abs() < expected * 0.015,
                "st={st}: shifted to {f} Hz, expected {expected:.2}"
            );
        }
    }

    #[test]
    fn pitch_path_output_stays_periodic() {
        // Identity locking must not smear a clean tone into noise.
        let sig = ts::harmonic_stack(220.0, 12, 0.5, 24_000, RATE);
        let out = run_pitch(&sig, 3.0);
        let core = &out[1000..out.len() - 1000];
        let f = ts::band_peak_freq(core, RATE, 240.0, 300.0).expect("peak");
        let expected = 220.0 * 2f64.powf(3.0 / 12.0);
        assert!((f - expected).abs() < expected * 0.01, "f={f}");
        // Periodicity: YIN f0 + autocorrelation clarity on the output.
        let (f0, clarity) = crate::analysis::yin_f0(core, RATE)
            .unwrap_or_else(|| panic!("no periodicity in output"));
        assert!(
            (f0 - expected).abs() < expected * 0.01,
            "yin f0 {f0:.1} vs {expected:.1}"
        );
        assert!(
            clarity > 0.8,
            "output lost periodicity (clarity {clarity:.3})"
        );
    }

    #[test]
    fn pitch_path_on_silence_is_silent_and_finite() {
        let sig = ts::silence(12_000);
        let out = run_pitch(&sig, 4.0);
        assert!(out.iter().all(|v| v.is_finite()));
        assert!(ts::max_abs(&out) < 1e-3, "silence produced energy");
    }
}
