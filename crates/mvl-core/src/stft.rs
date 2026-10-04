//! Streaming short-time Fourier analysis and overlap-add (OLA) synthesis.
//!
//! This is the shared front end of the whole DSP engine (architecture plan
//! §6.0): every processor — breath, formant, pitch — sees the *same*
//! analysis frames, which is what keeps the three treatments mutually
//! consistent.
//!
//! Conventions:
//!
//! * periodic Hann window, 75 % overlap (`hop = fft_size / 4`);
//! * the stream is *pre-padded* with `fft_size − hop` zeros ("left
//!   flush") so every real sample is covered by at least three frames —
//!   reconstruction, including the very first and last samples, is exact
//!   instead of edge-faded;
//! * synthesis divides by the running `Σ w²` denominator, so
//!   reconstruction is exact even where the window sum is not constant
//!   (signal edges, and the ±1-sample hop jitter of the phase-vocoder
//!   path);
//! * the FFT pair is unnormalised (realfft convention), so the synthesis
//!   side scales inverse-transform frames by `1 / fft_size`.

use std::collections::VecDeque;
use std::f64::consts::PI;
use std::sync::Arc;

use realfft::{RealFftPlanner, RealToComplex};
use rustfft::num_complex::Complex64;

/// One analysed frame: a windowed complex spectrum plus its stream position.
#[derive(Debug, Clone)]
pub struct AnalysisFrame {
    /// Start position of this frame in the *padded* stream (see
    /// [`StftAnalyzer::pad`]).
    pub start: usize,
    /// Windowed spectrum, `fft_size / 2 + 1` bins.
    pub spectrum: Vec<Complex64>,
}

/// Periodic Hann window: `w[k] = 0.5 − 0.5·cos(2πk/N)`.
///
/// The periodic form (not the symmetric one) satisfies the constant
/// overlap-add property for `w²` at 75 % overlap.
#[must_use]
pub fn hann_window(n: usize) -> Vec<f64> {
    (0..n)
        .map(|k| 0.5 - 0.5 * (2.0 * PI * k as f64 / n as f64).cos())
        .collect()
}

/// The OLA denominator `Σ_m w²[k − m·hop]` over `n_frames` frames of a
/// signal `len` samples long. Exposed for the unity-COLA unit test
/// (engine invariant #7, plan §6.7).
#[must_use]
pub fn cola_sum(win: &[f64], hop: usize, n_frames: usize, len: usize) -> Vec<f64> {
    let mut sum = vec![0.0f64; len];
    for m in 0..n_frames {
        let off = m * hop;
        for (k, w) in win.iter().enumerate() {
            if let Some(s) = sum.get_mut(off + k) {
                *s += w * w;
            }
        }
    }
    sum
}

/// Streaming forward STFT analyser.
///
/// Feed samples with [`StftAnalyzer::push`]; completed frames come back
/// from the same call. [`StftAnalyzer::flush`] emits the remaining tail
/// frames (zero-padded on the right) and marks the stream finished.
pub struct StftAnalyzer {
    fft_size: usize,
    hop: usize,
    win: Vec<f64>,
    r2c: Arc<dyn RealToComplex<f64>>,
    /// Padded-stream samples starting at `buf_start`.
    buf: VecDeque<f64>,
    buf_start: usize,
    next_frame: usize,
    total_input: usize,
    flushed: bool,
    scratch_in: Vec<f64>,
    scratch_spec: Vec<Complex64>,
}

impl StftAnalyzer {
    /// Create an analyser. `hop` must divide `fft_size` and be at most
    /// half of it (the engine always uses 75 % overlap, `hop = n / 4`).
    ///
    /// # Panics
    /// Panics on invalid `(fft_size, hop)` combinations — these come from
    /// the fixed `QualityProfile` table, never from user data.
    pub fn new(planner: &mut RealFftPlanner<f64>, fft_size: usize, hop: usize) -> Self {
        assert!(
            fft_size >= 8 && hop > 0 && hop <= fft_size / 2 && fft_size % hop == 0,
            "invalid STFT shape (n = {fft_size}, hop = {hop})"
        );
        let pad = fft_size - hop;
        let r2c = planner.plan_fft_forward(fft_size);
        let mut buf = VecDeque::with_capacity(fft_size * 4 + pad);
        for _ in 0..pad {
            buf.push_back(0.0);
        }
        Self {
            fft_size,
            hop,
            win: hann_window(fft_size),
            r2c,
            buf,
            buf_start: 0,
            next_frame: 0,
            total_input: 0,
            flushed: false,
            scratch_in: vec![0.0; fft_size],
            scratch_spec: vec![Complex64::new(0.0, 0.0); fft_size / 2 + 1],
        }
    }

    /// FFT size in samples.
    #[must_use]
    pub fn fft_size(&self) -> usize {
        self.fft_size
    }

    /// Hop in samples.
    #[must_use]
    pub fn hop(&self) -> usize {
        self.hop
    }

    /// Left-flush padding: the number of zero samples prepended to the
    /// stream. Real sample `j` lives at padded position `j + pad()`.
    #[must_use]
    pub fn pad(&self) -> usize {
        self.fft_size - self.hop
    }

    /// Real (unpadded) samples pushed so far.
    #[must_use]
    pub fn total_input(&self) -> usize {
        self.total_input
    }

    /// Feed real samples; returns all analysis frames that became complete.
    ///
    /// # Panics
    /// Panics if called after [`StftAnalyzer::flush`] (engine protocol
    /// violation, not user data).
    pub fn push(&mut self, samples: &[f32]) -> Vec<AnalysisFrame> {
        assert!(!self.flushed, "push after flush");
        for &s in samples {
            self.buf.push_back(f64::from(s));
        }
        self.total_input += samples.len();
        self.emit_ready()
    }

    /// Finish the stream: zero-pad the right side and emit every frame
    /// that still starts inside the real signal.
    pub fn flush(&mut self) -> Vec<AnalysisFrame> {
        if self.flushed {
            return Vec::new();
        }
        self.flushed = true;
        let stream_end = self.pad() + self.total_input;
        // Enough padding for the last in-stream frame plus one beyond.
        let needed = stream_end + self.fft_size;
        while self.buf_start + self.buf.len() < needed {
            self.buf.push_back(0.0);
        }
        self.emit_ready()
    }

    fn emit_ready(&mut self) -> Vec<AnalysisFrame> {
        let stream_end = self.pad() + self.total_input;
        let mut frames = Vec::new();
        loop {
            if self.next_frame >= stream_end {
                break;
            }
            let rel = self.next_frame - self.buf_start;
            if rel + self.fft_size > self.buf.len() {
                break;
            }
            // Window + forward transform (input doubles as FFT scratch).
            for i in 0..self.fft_size {
                self.scratch_in[i] = self.win[i] * self.buf[rel + i];
            }
            let mut spec = std::mem::take(&mut self.scratch_spec);
            // Lengths are pre-validated by construction.
            self.r2c
                .process(&mut self.scratch_in, &mut spec)
                .expect("stft lengths");
            self.scratch_spec = std::mem::take(&mut spec.clone());
            frames.push(AnalysisFrame {
                start: self.next_frame,
                spectrum: spec,
            });
            self.next_frame += self.hop;
        }
        // Retire samples that no future frame can need.
        let keep = self.next_frame - self.buf_start;
        if keep > 0 {
            self.buf.drain(..keep);
            self.buf_start = self.next_frame;
        }
        frames
    }
}

/// Streaming overlap-add synthesiser with an exact running denominator.
///
/// Frames are added at explicit absolute positions (which may advance by
/// a non-constant hop, as in the phase-vocoder path). Samples below the
/// current *frontier* (the start of the most recently added frame) are
/// final and can be popped.
pub struct OverlapAdder {
    n: usize,
    win: Vec<f64>,
    acc: VecDeque<f64>,
    den: VecDeque<f64>,
    /// Absolute position of `acc[0]`.
    base: usize,
    /// Absolute position of the next sample to emit.
    emitted: usize,
    /// Absolute position of the newest frame start; samples below it are final.
    frontier: usize,
}

impl OverlapAdder {
    /// Create a synthesiser for frames of `n` samples, windowed with `win`
    /// (the *synthesis* window, applied here — callers pass plain
    /// inverse-transform frames).
    #[must_use]
    pub fn new(n: usize, win: Vec<f64>) -> Self {
        assert_eq!(n, win.len(), "window/frame length mismatch");
        Self {
            n,
            win,
            acc: VecDeque::new(),
            den: VecDeque::new(),
            base: 0,
            emitted: 0,
            frontier: 0,
        }
    }

    /// Add one time-domain frame (length `n`, unwindowed) at absolute
    /// padded-stream position `pos`. Positions must be non-decreasing and
    /// never below the already-emitted boundary.
    ///
    /// # Panics
    /// Panics if `frame.len() != n` or if `pos` rewinds below the emit
    /// frontier — both are engine protocol violations.
    pub fn add_frame_at(&mut self, frame: &[f64], pos: usize) {
        assert_eq!(frame.len(), self.n, "frame length mismatch");
        assert!(
            pos >= self.emitted,
            "frame position {pos} rewinds below emitted {}",
            self.emitted
        );
        let end = pos + self.n;
        while self.base + self.acc.len() < end {
            self.acc.push_back(0.0);
            self.den.push_back(0.0);
        }
        let off = pos - self.base;
        for (i, (&f, &w)) in frame.iter().zip(&self.win).enumerate() {
            let idx = off + i;
            self.acc[idx] += w * f;
            self.den[idx] += w * w;
        }
        self.frontier = self.frontier.max(pos);
    }

    /// Pop finalised samples at absolute positions `[emitted, limit)`.
    /// `limit` is clamped to the frontier and to what has been written.
    #[must_use]
    pub fn pop_ready(&mut self, limit: usize) -> Vec<f32> {
        let limit = limit.min(self.frontier).min(self.base + self.acc.len());
        self.pop_upto(limit)
    }

    /// Pop everything up to `limit`, treating gaps and unfinalised regions
    /// as zero (end-of-stream drain; missing coverage yields silence).
    #[must_use]
    pub fn drain(&mut self, limit: usize) -> Vec<f32> {
        while self.base + self.acc.len() < limit {
            self.acc.push_back(0.0);
            self.den.push_back(0.0);
        }
        self.pop_upto(limit)
    }

    fn pop_upto(&mut self, limit: usize) -> Vec<f32> {
        let count = limit.saturating_sub(self.emitted);
        let mut out = Vec::with_capacity(count);
        for _ in 0..count {
            let a = self.acc.pop_front().unwrap_or(0.0);
            let d = self.den.pop_front().unwrap_or(0.0);
            let v = if d > 1e-12 { a / d } else { 0.0 };
            out.push(v as f32);
        }
        self.emitted += count;
        self.base = self.emitted;
        out
    }

    /// Absolute position of the next sample to emit.
    #[must_use]
    pub fn emitted(&self) -> usize {
        self.emitted
    }

    /// Current frontier: samples at positions below it are final.
    #[must_use]
    pub fn frontier(&self) -> usize {
        self.frontier
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic LCG noise in [−1, 1).
    fn lcg(n: usize, seed: u64) -> Vec<f32> {
        let mut s = seed;
        (0..n)
            .map(|_| {
                s = s
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                ((s >> 33) as f32 / (u32::MAX >> 1) as f32) - 1.0
            })
            .collect()
    }

    /// Full analysis → (unmodified) synthesis round trip.
    fn roundtrip(samples: &[f32], n: usize, hop: usize) -> Vec<f32> {
        let mut planner = RealFftPlanner::new();
        let mut an = StftAnalyzer::new(&mut planner, n, hop);
        let c2r = planner.plan_fft_inverse(n);
        let mut ola = OverlapAdder::new(n, hann_window(n));

        let mut frames = an.push(samples);
        frames.extend(an.flush());
        assert!(!frames.is_empty() || samples.is_empty());
        for f in &frames {
            let mut spec = f.spectrum.clone();
            let mut time = vec![0.0f64; n];
            c2r.process(&mut spec, &mut time).unwrap();
            let scale = 1.0 / n as f64;
            for v in &mut time {
                *v *= scale;
            }
            ola.add_frame_at(&time, f.start);
        }
        let pad = n - hop;
        let mut out = ola.drain(pad + samples.len());
        out.drain(..pad);
        out.truncate(samples.len());
        out
    }

    fn max_err(a: &[f32], b: &[f32]) -> f32 {
        a.iter()
            .zip(b)
            .map(|(x, y)| (x - y).abs())
            .fold(0.0, f32::max)
    }

    #[test]
    fn hann_matches_reference_values() {
        let w = hann_window(8);
        assert!((w[0]).abs() < 1e-12);
        assert!((w[2] - 0.5).abs() < 1e-12);
        assert!((w[4] - 1.0).abs() < 1e-12);
        assert!((w[6] - 0.5).abs() < 1e-12);
        // cos(7π/4) = +√2/2 → w[7] = 0.5 − √2/4 ≈ 0.146447.
        assert!((w[7] - 0.146_446_609_406_726_2).abs() < 1e-12);
    }

    #[test]
    fn cola_unity_at_75_percent_overlap() {
        // Engine invariant #7: Hann² at 75 % overlap sums to a constant.
        let n = 512;
        let hop = n / 4;
        let win = hann_window(n);
        let frames = 12;
        let sum = cola_sum(&win, hop, frames, n + (frames - 1) * hop);
        // Interior region covered by all four overlapping frames.
        for (k, &s) in sum.iter().enumerate() {
            if (n..((frames - 1) * hop)).contains(&k) {
                assert!(
                    (s - 1.5).abs() < 1e-9,
                    "COLA sum drifted at {k}: {s} (expected 1.5)"
                );
            }
        }
    }

    #[test]
    fn identity_reconstruction_both_profile_shapes() {
        // Preview shape (512/128) and render shape (2048/512).
        for &(n, hop, len) in &[(512, 128, 5000), (2048, 512, 9000)] {
            let input = lcg(len, 0xC0FFEE);
            let out = roundtrip(&input, n, hop);
            assert_eq!(out.len(), len);
            let err = max_err(&input, &out);
            assert!(err < 1e-9, "n={n}: max error {err:e}");
        }
    }

    #[test]
    fn streaming_chunking_matches_bulk() {
        let input = lcg(4000, 42);
        let bulk = roundtrip(&input, 512, 128);

        let mut planner = RealFftPlanner::new();
        let mut an = StftAnalyzer::new(&mut planner, 512, 128);
        let mut collected = Vec::new();
        for chunk in input.chunks(173) {
            collected.extend(an.push(chunk));
        }
        collected.extend(an.flush());

        let mut planner2 = RealFftPlanner::new();
        let mut an2 = StftAnalyzer::new(&mut planner2, 512, 128);
        let mut frames = an2.push(&input);
        frames.extend(an2.flush());

        assert_eq!(collected.len(), frames.len());
        for (a, b) in collected.iter().zip(&frames) {
            assert_eq!(a.start, b.start);
            assert_eq!(a.spectrum.len(), b.spectrum.len());
        }
        assert_eq!(max_err(&bulk, &roundtrip(&input, 512, 128)), 0.0);
    }

    #[test]
    fn short_signal_reconstructs_exactly() {
        // Shorter than one frame: the left-flush pad still makes it exact.
        let input = lcg(137, 7);
        let out = roundtrip(&input, 512, 128);
        assert_eq!(out.len(), 137);
        assert!(max_err(&input, &out) < 1e-9);
    }

    #[test]
    fn empty_input_is_empty_output() {
        let out = roundtrip(&[], 512, 128);
        assert!(out.is_empty());
    }

    #[test]
    fn second_flush_returns_nothing() {
        let mut planner = RealFftPlanner::new();
        let mut an = StftAnalyzer::new(&mut planner, 512, 128);
        an.push(&lcg(2000, 99));
        let a = an.flush().len();
        assert!(a > 0);
        assert!(an.flush().is_empty());
    }

    #[test]
    fn ola_denominator_guard_covers_gaps() {
        // drain() beyond written coverage yields silence, not NaN.
        let mut ola = OverlapAdder::new(8, hann_window(8));
        ola.add_frame_at(&[1.0; 8], 0);
        let out = ola.drain(64);
        assert_eq!(out.len(), 64);
        assert!(out.iter().all(|v| v.is_finite()));
    }
}
