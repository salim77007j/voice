//! The vocal processing engine (plan §6.5): one shared STFT analysis
//! feeding the three precision processors, assembled into a single
//! streaming chain.
//!
//! ```text
//! Source f32
//!   → [STFT analysis]   (shared: F0, voicing, features, envelope)
//!   → [1] Air/Breath    (spectral gain field, breath/sibilant frames only)
//!   → [2] Formant       (envelope warp: g_user / r_pitch compensation)
//!   → [3] Pitch         (PV-TSM × r → sinc ratio × 1/r → original duration)
//!   → [4] True-peak safety guard
//!   → Output f32
//! ```
//!
//! Everything is `f32` in / `f32` out with `f64` internal math. The
//! engine is mono (one channel of state); use [`VocalEngine::render_interleaved`]
//! for multi-channel material — each channel gets its own engine with
//! the same parameters.
//!
//! **Neutral bypass:** while parameters have never left the neutral
//! set, [`VocalEngine::process`] returns its input unchanged
//! (bit-exact, invariant #1). Once the chain has engaged, returning to
//! neutral parameters runs the (then no-op) chain instead of bypassing,
//! because the STFT state is live.
//!
//! **Live parameter changes:** air and formant are frame-local fields
//! and follow parameter changes at the next hop boundary (the 75 %
//! OLA overlap crossfades them inherently). Pitch ratio changes rebuild
//! the TSM path — a small transient at the switch point; a click-free
//! parameter crossfade for the pitch path is Phase 4 polish (noted
//! honestly).

use std::sync::Arc;

use realfft::{ComplexToReal, RealFftPlanner};
use rustfft::num_complex::Complex64;

use crate::analysis::{FrameAnalyzer, FrameFeatures};
use crate::breath::BreathProcessor;
use crate::error::EngineError;
use crate::formant::FormantProcessor;
use crate::limiter;
use crate::params::{QualityProfile, VocalParams};
use crate::pitch::PitchPath;
use crate::stft::{hann_window, AnalysisFrame, OverlapAdder, StftAnalyzer};
use crate::ClassSmoother;

/// Offline render result with the evidence the UI and tests need.
#[derive(Debug, Clone)]
pub struct RenderResult {
    /// Processed output, exactly as many samples as the input.
    pub output: Vec<f32>,
    /// Per-frame analysis of the *source* (raw classifications).
    pub frames: Vec<FrameFeatures>,
    /// Whether the true-peak guard had to engage (asserted `false` for
    /// moderate settings by the invariant suite).
    pub limiter_engaged: bool,
    /// Deepest air/breath gain applied (dB, ≤ 0). The 0.1 dB readout.
    pub applied_air_db: f32,
}

/// The streaming vocal engine.
pub struct VocalEngine {
    rate: u32,
    profile: QualityProfile,
    n: usize,
    hop: usize,
    bins: usize,
    pad: usize,

    planner: RealFftPlanner<f64>,
    stft: StftAnalyzer,
    analyzer: FrameAnalyzer,
    smoother: ClassSmoother,
    breath: BreathProcessor,
    formant: FormantProcessor,
    c2r: Arc<dyn ComplexToReal<f64>>,
    ola: OverlapAdder,

    pitch: Option<PitchPath>,
    pitch_ratio: f64,
    pad_drop: usize,

    params: VocalParams,
    engaged: bool,
    flushed: bool,
    src_peak: f32,
    guard_engaged: bool,

    frames: Vec<FrameFeatures>,
    applied_air_db: f64,

    chain_in: usize,
    chain_out: usize,
    scratch_spec: Vec<Complex64>,
    scratch_time: Vec<f64>,
}

impl VocalEngine {
    /// Create an engine for a session sample rate and quality profile.
    ///
    /// # Errors
    /// [`EngineError::ZeroRate`] for a zero rate; construction otherwise
    /// cannot fail on valid rates.
    pub fn new(sample_rate: u32, profile: QualityProfile) -> Result<Self, EngineError> {
        if sample_rate == 0 {
            return Err(EngineError::ZeroRate(sample_rate));
        }
        let n = profile.fft_size();
        let hop = profile.hop_size();
        let bins = n / 2 + 1;
        let mut planner = RealFftPlanner::new();
        let stft = StftAnalyzer::new(&mut planner, n, hop);
        let c2r = planner.plan_fft_inverse(n);
        let formant = FormantProcessor::new(&mut planner, n, sample_rate, hop);
        let smoother = ClassSmoother::new(match profile {
            QualityProfile::Preview => 3,
            QualityProfile::Render => 7,
        });
        Ok(Self {
            rate: sample_rate,
            profile,
            n,
            hop,
            bins,
            pad: n - hop,
            planner,
            stft,
            analyzer: FrameAnalyzer::new(sample_rate),
            smoother,
            breath: BreathProcessor::new(bins, sample_rate, hop),
            formant,
            c2r,
            ola: OverlapAdder::new(n, hann_window(n)),
            pitch: None,
            pitch_ratio: 1.0,
            pad_drop: n - hop,
            params: VocalParams::neutral(),
            engaged: false,
            flushed: false,
            src_peak: 0.0,
            guard_engaged: false,
            frames: Vec::new(),
            applied_air_db: 0.0,
            chain_in: 0,
            chain_out: 0,
            scratch_spec: Vec::with_capacity(bins),
            scratch_time: vec![0.0; n],
        })
    }

    /// Session sample rate.
    #[must_use]
    pub fn sample_rate(&self) -> u32 {
        self.rate
    }

    /// Quality profile in use.
    #[must_use]
    pub fn profile(&self) -> QualityProfile {
        self.profile
    }

    /// Estimated algorithmic latency in samples (informational; the
    /// invariant suite measures the real thing).
    #[must_use]
    pub fn output_delay(&self) -> usize {
        match &self.pitch {
            Some(p) => self.n + p.total_delay(),
            None => self.n,
        }
    }

    /// Whether the true-peak guard engaged so far.
    #[must_use]
    pub fn guard_engaged(&self) -> bool {
        self.guard_engaged
    }

    /// Process one block. Returns every output sample currently
    /// available (may be fewer than the input while the pipeline fills,
    /// and is the input unchanged while parameters have stayed neutral).
    ///
    /// # Errors
    /// [`EngineError::NonFiniteInput`] on NaN/Inf; [`EngineError::Internal`]
    /// if called after [`VocalEngine::flush`].
    pub fn process(&mut self, input: &[f32], params: VocalParams) -> Result<Vec<f32>, EngineError> {
        if self.flushed {
            return Err(EngineError::Internal("process after flush".into()));
        }
        if let Some(i) = input.iter().position(|s| !s.is_finite()) {
            return Err(EngineError::NonFiniteInput(i));
        }
        let params = params.sanitized();

        let in_peak = input.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        self.src_peak = self.src_peak.max(in_peak);

        if !self.engaged && params.is_neutral() {
            // Bit-exact bypass (invariant #1).
            return Ok(input.to_vec());
        }
        self.engaged = true;
        self.params = params;
        self.chain_in += input.len();

        self.manage_pitch_path(&params)?;

        // Feed the analysis layer at hop granularity, interleaved with
        // frame processing: the analyser's rolling history and
        // sub-window transient tracker must advance in step with the
        // frames they describe (a single bulk push would trim history
        // the pending frames still need, and misalign transient flags).
        // This also makes bulk `render` and streamed `process`
        // bit-identical by construction.
        for piece in input.chunks(self.hop) {
            self.analyzer.push_history(piece);
            let frames = self.stft.push(piece);
            self.run_frames(frames, &params);
        }

        let mut out = self.collect_output(input.len() + 4096);
        self.guard_block(&mut out);
        self.chain_out += out.len();
        Ok(out)
    }

    /// End of stream: flush the STFT, drain the synthesis paths and
    /// return the remaining output. The stream total (all `process`
    /// returns plus this) is exactly the number of input samples.
    ///
    /// # Errors
    /// [`EngineError::Internal`] if called twice.
    pub fn flush(&mut self) -> Result<Vec<f32>, EngineError> {
        if self.flushed {
            return Err(EngineError::Internal("double flush".into()));
        }
        self.flushed = true;
        if !self.engaged {
            return Ok(Vec::new());
        }
        let params = self.params;
        let frames = self.stft.flush();
        self.run_frames(frames, &params);

        let remaining = self.chain_in - self.chain_out;
        let mut out = if let Some(path) = &mut self.pitch {
            path.flush(self.chain_in)
        } else {
            let mut out = self.ola.drain(self.pad + self.chain_in);
            if self.pad_drop > 0 {
                let d = self.pad_drop.min(out.len());
                out.drain(..d);
                self.pad_drop -= d;
            }
            if out.len() >= remaining {
                out.truncate(remaining);
            } else {
                out.resize(remaining, 0.0);
            }
            out
        };
        self.guard_block(&mut out);
        self.chain_out += out.len();
        Ok(out)
    }

    /// Offline one-shot render with the full-quality guard and the
    /// analysis record.
    ///
    /// # Errors
    /// Propagates [`VocalEngine::process`] errors.
    pub fn render(
        input: &[f32],
        sample_rate: u32,
        params: VocalParams,
        profile: QualityProfile,
    ) -> Result<RenderResult, EngineError> {
        let mut engine = Self::new(sample_rate, profile)?;
        let mut output = engine.process(input, params)?;
        output.extend(engine.flush()?);
        if output.len() > input.len() {
            output.truncate(input.len());
        }
        while output.len() < input.len() {
            output.push(0.0);
        }
        let src_tp = limiter::true_peak(input);
        let engaged = limiter::guard_offline(&mut output, src_tp);
        Ok(RenderResult {
            output,
            frames: engine.frames,
            limiter_engaged: engaged || engine.guard_engaged,
            applied_air_db: engine.applied_air_db as f32,
        })
    }

    /// Convenience: render interleaved multi-channel audio (one engine
    /// per channel, same parameters).
    ///
    /// # Errors
    /// Propagates [`VocalEngine::render`] errors; [`EngineError::Internal`]
    /// for zero channels or a non-whole number of frames.
    pub fn render_interleaved(
        data: &[f32],
        channels: u16,
        sample_rate: u32,
        params: VocalParams,
        profile: QualityProfile,
    ) -> Result<Vec<f32>, EngineError> {
        let ch = channels as usize;
        if ch == 0 {
            return Err(EngineError::Internal("zero channel count".into()));
        }
        if data.len() % ch != 0 {
            return Err(EngineError::Internal(
                "buffer is not a whole number of frames".into(),
            ));
        }
        let frames = data.len() / ch;
        let mut deinterleaved: Vec<Vec<f32>> = Vec::with_capacity(ch);
        for c in 0..ch {
            deinterleaved.push((0..frames).map(|i| data[i * ch + c]).collect());
        }
        let mut outs = Vec::with_capacity(ch);
        for chan in &deinterleaved {
            outs.push(Self::render(chan, sample_rate, params, profile)?.output);
        }
        let mut out = Vec::with_capacity(data.len());
        for i in 0..frames {
            for o in &outs {
                out.push(o[i]);
            }
        }
        Ok(out)
    }

    fn manage_pitch_path(&mut self, params: &VocalParams) -> Result<(), EngineError> {
        let want = f64::from(params.pitch_ratio());
        if params.pitch_semitones == 0.0 {
            if self.pitch.take().is_some() {
                // Mid-stream disengagement: the identity OLA continues
                // from the current stream position. The switch point has
                // a small transient (documented Phase 3 limitation).
                self.pitch_ratio = 1.0;
                self.pad_drop = 0;
            }
        } else if self.pitch.is_none() {
            let path = PitchPath::new(&mut self.planner, self.n, self.hop, want)?;
            self.pitch = Some(path);
            self.pitch_ratio = want;
            self.pad_drop = 0;
        } else if (self.pitch_ratio - want).abs() > 1e-9 {
            // Ratio change: rebuild (transient at the switch, Phase 4
            // will crossfade).
            let path = PitchPath::new(&mut self.planner, self.n, self.hop, want)?;
            self.pitch = Some(path);
            self.pitch_ratio = want;
        }
        Ok(())
    }

    fn run_frames(&mut self, frames: Vec<AnalysisFrame>, params: &VocalParams) {
        for frame in frames {
            let feats = self
                .analyzer
                .analyze(frame.start + self.hop, &frame.spectrum, self.n);
            let class = self.smoother.push(feats.class);
            let f0 = feats.f0_hz;

            // Stage 1: air & breath.
            let edit = self
                .breath
                .process_frame(&frame.spectrum, &feats, class, params);
            self.applied_air_db = self.applied_air_db.min(self.breath.last_applied_db());

            // Stage 2: formant warp with pitch compensation (the
            // anti-chipmunk contract: net displacement = user's slider).
            let g_frame = f64::from(params.formant_ratio()) / f64::from(params.pitch_ratio());
            let fmult = self
                .formant
                .warp_field(&frame.spectrum, g_frame, f0.map(f64::from));

            // Combined modification: multiplicative fields + air injection.
            let spec = &frame.spectrum;
            self.scratch_spec.clear();
            for k in 0..self.bins {
                let g = edit.gain[k] * fmult[k];
                let a = &edit.additive[k];
                self.scratch_spec
                    .push(Complex64::new(spec[k].re * g + a.re, spec[k].im * g + a.im));
            }
            self.scratch_spec[0].im = 0.0;
            let last = self.bins - 1;
            self.scratch_spec[last].im = 0.0;

            // Stage 3: pitch path or identity synthesis. Phase locking
            // follows the *raw* periodicity evidence (clarity + valid f0),
            // not the classification: the classifier's Silence fallback
            // for stream-edge frames must not downgrade locking — the
            // phase-propagation memory would carry the perturbation
            // through the whole stream (found by the invariant tests).
            let voiced = feats.f0_hz.is_some() && feats.clarity >= crate::analysis::VOICED_CLARITY;
            if let Some(path) = &mut self.pitch {
                path.push_frame(&self.scratch_spec, voiced);
            } else {
                let mut time = std::mem::take(&mut self.scratch_time);
                self.c2r
                    .process(&mut self.scratch_spec, &mut time)
                    .expect("pre-sized buffers");
                let scale = 1.0 / self.n as f64;
                for v in time.iter_mut() {
                    *v *= scale;
                }
                self.ola.add_frame_at(&time, frame.start);
                self.scratch_time = time;
            }

            self.frames.push(feats);
        }
    }

    fn collect_output(&mut self, max: usize) -> Vec<f32> {
        if let Some(path) = &mut self.pitch {
            path.pop_output(max)
        } else {
            let frontier = self.ola.frontier();
            let mut out = self.ola.pop_ready(frontier);
            if self.pad_drop > 0 {
                let d = self.pad_drop.min(out.len());
                out.drain(..d);
                self.pad_drop -= d;
            }
            out
        }
    }

    /// Streaming guard: absolute −0.1 dBTP sample-peak ceiling per
    /// emitted block (source-relative bound is offline-only, see module
    /// docs).
    fn guard_block(&mut self, out: &mut [f32]) {
        let ceiling = 10.0f64.powf(limiter::CEILING_DBTP / 20.0);
        let peak = limiter::sample_peak(out);
        if peak > ceiling && peak > 0.0 {
            let g = (ceiling / peak) as f32;
            for v in out.iter_mut() {
                *v *= g;
            }
            self.guard_engaged = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsupport as ts;

    const RATE: u32 = 48_000;

    fn vowel() -> Vec<f32> {
        ts::formant_vowel(
            196.0,
            &[(730.0, 10.0), (1090.0, 10.0), (2500.0, 12.0)],
            0.5,
            48_000,
            RATE,
        )
    }

    fn stack() -> Vec<f32> {
        ts::harmonic_stack(220.0, 12, 0.5, 24_000, RATE)
    }

    /// F0 of a rendered output: dominant spectral peak around the
    /// expected value. Chosen over YIN because extreme pitch shifts
    /// carry slow PV phase modulation that can fool period-based
    /// estimators onto subharmonics, while the spectral peak stays
    /// exact.
    fn f0_near(x: &[f32], expected: f64) -> f64 {
        let core = &x[2000..x.len() - 2000];
        let lo = expected * 0.94;
        let hi = expected * 1.06;
        ts::band_peak_freq(core, RATE, lo, hi)
            .unwrap_or_else(|| panic!("no spectral peak near {expected:.1} Hz"))
    }

    /// Input F0 via YIN (source is clean).
    fn f0_of(x: &[f32]) -> f64 {
        let core = &x[2000..x.len() - 2000];
        crate::analysis::yin_f0(core, RATE)
            .unwrap_or_else(|| panic!("no f0 in output"))
            .0
    }

    #[test]
    fn invariant_1_neutral_render_is_bit_exact() {
        for sig in [vowel(), stack(), ts::silence(4800)] {
            let out =
                VocalEngine::render(&sig, RATE, VocalParams::neutral(), QualityProfile::Preview)
                    .unwrap();
            assert_eq!(out.output, sig, "neutral render must be bit-exact");
            assert!(!out.limiter_engaged);
        }
    }

    #[test]
    fn invariant_2_pitch_shifts_f0_and_keeps_duration() {
        for st in [7.0f32, -5.0, 12.0, -12.0] {
            let params = VocalParams {
                pitch_semitones: st,
                ..VocalParams::neutral()
            };
            let out = VocalEngine::render(&stack(), RATE, params, QualityProfile::Preview).unwrap();
            assert_eq!(
                out.output.len(),
                stack().len(),
                "duration changed at {st} st"
            );
            let expected = 220.0 * 2f64.powf(f64::from(st) / 12.0);
            let got = f0_near(&out.output, expected);
            assert!(
                (got - expected).abs() < expected * 0.005,
                "st={st}: f0 {got:.2} vs {expected:.2}"
            );
        }
    }

    #[test]
    fn invariant_3_formants_survive_pitch_shift() {
        // The anti-chipmunk guarantee: pitch +12 with neutral formants
        // leaves the spectral envelope (F1/F2 zones) in place. F1 uses
        // a tight band centroid; F2 uses the harmonic centroid (pitch
        // doubling re-grids the harmonics, which biases plain band
        // centroids — the harmonic metric samples the envelope at each
        // signal's own partials).
        let sig = vowel();
        let params = VocalParams {
            pitch_semitones: 12.0,
            ..VocalParams::neutral()
        };
        let out = VocalEngine::render(&sig, RATE, params, QualityProfile::Preview).unwrap();
        let core = sig.len() / 4;
        let (lo, hi) = (550.0, 950.0);
        let c_in = ts::band_centroid(&sig[core..sig.len() - core], RATE, lo, hi);
        let c_out = ts::band_centroid(&out.output[core..out.output.len() - core], RATE, lo, hi);
        assert!(
            (c_out / c_in - 1.0).abs() < 0.03,
            "F1 zone {lo}-{hi} Hz moved {c_in:.0} -> {c_out:.0} (>3 %) — chipmunk leak"
        );
        // F2 on a dense-grid variant (f0 98 Hz in, 196 Hz out): both
        // grids sample the F2 envelope finely enough that re-gridding
        // cannot bias a band centroid.
        let dense = ts::formant_vowel(
            98.0,
            &[(730.0, 10.0), (1090.0, 10.0), (2500.0, 12.0)],
            0.5,
            48_000,
            RATE,
        );
        let out_d = VocalEngine::render(&dense, RATE, params, QualityProfile::Render).unwrap();
        let core_d = dense.len() / 4;
        let c2_in =
            ts::third_octave_centroid(&dense[core_d..dense.len() - core_d], RATE, 850.0, 1800.0);
        let c2_out = ts::third_octave_centroid(
            &out_d.output[core_d..out_d.output.len() - core_d],
            RATE,
            850.0,
            1800.0,
        );
        assert!(
            (c2_out / c2_in - 1.0).abs() < 0.04,
            "F2 zone moved {c2_in:.0} -> {c2_out:.0} (>4 %) — chipmunk leak"
        );
    }

    #[test]
    fn invariant_4_formant_shift_keeps_f0() {
        let sig = vowel();
        let params = VocalParams {
            tract_mm: 130.0, // g = 170/130 = 1.308
            ..VocalParams::neutral()
        };
        let out = VocalEngine::render(&sig, RATE, params, QualityProfile::Preview).unwrap();
        let f_in = f0_of(&sig);
        let f_out = f0_of(&out.output);
        assert!(
            (f_out - f_in).abs() < f_in * 0.001,
            "formant shift moved F0: {f_in:.2} -> {f_out:.2}"
        );
    }

    #[test]
    fn formant_shift_moves_envelope_by_g() {
        let sig = vowel();
        let params = VocalParams {
            tract_mm: 140.0, // g = 170/140 = 1.214
            ..VocalParams::neutral()
        };
        // Render profile: the 2048-pt analysis resolves the 196 Hz
        // harmonics cleanly (the 512-pt preview frame resolves them only
        // marginally, which measurably blunts the warp — documented
        // preview trade-off, plan section 6.6; verified separately).
        let out = VocalEngine::render(&sig, RATE, params, QualityProfile::Render).unwrap();
        let core = sig.len() / 4;
        let g = 170.0f64 / 140.0;
        // Harmonic centroid over wide scaled bands (no pitch change, so
        // both sides sample the envelope on the same 196 Hz grid).
        let c_in = ts::harmonic_centroid(&sig[core..sig.len() - core], RATE, 196.0, 300.0, 1600.0);
        let c_out = ts::harmonic_centroid(
            &out.output[core..out.output.len() - core],
            RATE,
            196.0,
            300.0,
            1600.0 * g,
        );
        assert!(
            (c_out / c_in - g).abs() < 0.05,
            "F1 harmonic centroid ratio {:.3} vs g {g:.3}",
            c_out / c_in
        );

        // Preview delivery is lower but bounded (measured ~95 % of g).
        let out_p = VocalEngine::render(&sig, RATE, params, QualityProfile::Preview).unwrap();
        let c_p = ts::harmonic_centroid(
            &out_p.output[core..out_p.output.len() - core],
            RATE,
            196.0,
            300.0,
            1600.0 * g,
        );
        assert!(
            (c_p / c_in - g).abs() < 0.09,
            "preview F1 centroid ratio {:.3} vs g {g:.3}",
            c_p / c_in
        );
    }

    #[test]
    fn invariant_5_air_removal_preserves_voiced() {
        // Breath segment: -40 dB; voiced segment: untouched.
        let breath = ts::breath_noise(31, RATE as usize, RATE, 0.063);
        let voiced = stack();
        let params = VocalParams {
            air_percent: -100,
            ..VocalParams::neutral()
        };
        let out = VocalEngine::render(&breath, RATE, params, QualityProfile::Preview).unwrap();
        let core = breath.len() / 4;
        let d_breath = ts::rms_db(&out.output[core..out.output.len() - core])
            - ts::rms_db(&breath[core..breath.len() - core]);
        assert!(
            (d_breath - -40.0).abs() < 0.5,
            "breath reduction {d_breath:.2} dB"
        );
        assert!(
            (out.applied_air_db - -40.0).abs() < 0.05,
            "applied readout {} dB",
            out.applied_air_db
        );

        let out_v = VocalEngine::render(&voiced, RATE, params, QualityProfile::Preview).unwrap();
        let core_v = voiced.len() / 4;
        let d_voiced = ts::rms_db(&out_v.output[core_v..out_v.output.len() - core_v])
            - ts::rms_db(&voiced[core_v..voiced.len() - core_v]);
        assert!(
            d_voiced.abs() < 0.1,
            "voiced energy moved {d_voiced:.2} dB under full breath removal"
        );
    }

    #[test]
    fn invariant_6_no_nan_across_parameter_grid() {
        // Short signals keep the grid fast while still covering every
        // stage combination (81 renders).
        let vowel_s = ts::formant_vowel(196.0, &[(730.0, 10.0), (1090.0, 10.0)], 0.5, 12_000, RATE);
        let signals: Vec<Vec<f32>> = vec![
            vowel_s,
            ts::harmonic_stack(220.0, 12, 0.5, 12_000, RATE),
            ts::breath_noise(41, 12_000, RATE, 0.05),
            ts::silence(12_000),
        ];
        let mut combos = 0;
        for &pitch in &[-12.0f32, 0.0, 5.0] {
            for &air in &[-100i32, 0, 100] {
                for &tract in &[100.0f32, 170.0, 260.0] {
                    let params = VocalParams {
                        pitch_semitones: pitch,
                        air_percent: air,
                        tract_mm: tract,
                    };
                    for sig in &signals {
                        let out = VocalEngine::render(sig, RATE, params, QualityProfile::Preview)
                            .unwrap();
                        assert_eq!(out.output.len(), sig.len());
                        assert!(
                            out.output.iter().all(|v| v.is_finite()),
                            "non-finite output at pitch={pitch} air={air} tract={tract}"
                        );
                        combos += 1;
                    }
                }
            }
        }
        assert!(combos >= 100, "grid too small: {combos}");
    }

    #[test]
    fn pitch_and_formant_combined() {
        // Both sliders: f0 moves by r AND envelope by g, independently.
        let sig = vowel();
        let params = VocalParams {
            pitch_semitones: 5.0, // r = 1.335
            tract_mm: 120.0,      // g = 1.417
            ..VocalParams::neutral()
        };
        let out = VocalEngine::render(&sig, RATE, params, QualityProfile::Render).unwrap();
        assert_eq!(out.output.len(), sig.len());
        let f_expected = 196.0 * 2f64.powf(5.0 / 12.0);
        let got = f0_near(&out.output, f_expected);
        assert!(
            (got - f_expected).abs() < f_expected * 0.005,
            "combined: f0 {got:.1} vs {f_expected:.1}"
        );
        let core = sig.len() / 4;
        let g = 170.0f64 / 120.0;
        let r = 2f64.powf(5.0 / 12.0);
        let c_in = ts::harmonic_centroid(&sig[core..sig.len() - core], RATE, 196.0, 300.0, 1600.0);
        let c_out = ts::harmonic_centroid(
            &out.output[core..out.output.len() - core],
            RATE,
            196.0 * r,
            300.0,
            1600.0 * g,
        );
        assert!(
            (c_out / c_in - g).abs() < 0.07,
            "combined: envelope ratio {:.3} vs g {g:.3}",
            c_out / c_in
        );
    }

    #[test]
    fn limiter_engages_only_on_overshoot() {
        // Moderate settings on a normal-level source: never engages.
        let sig = ts::formant_vowel(196.0, &[(730.0, 10.0), (1090.0, 10.0)], 0.5, 24_000, RATE);
        let params = VocalParams {
            air_percent: 30,
            ..VocalParams::neutral()
        };
        let out = VocalEngine::render(&sig, RATE, params, QualityProfile::Preview).unwrap();
        assert!(!out.limiter_engaged, "limiter engaged on moderate settings");

        // +100 % air on a 12 kHz sine at 0.95 (entirely inside the
        // shelf band): +6 dB overshoot, deterministic.
        let loud = ts::sine(12_000.0, 0.95, 24_000, RATE);
        let params = VocalParams {
            air_percent: 100,
            ..VocalParams::neutral()
        };
        let out = VocalEngine::render(&loud, RATE, params, QualityProfile::Preview).unwrap();
        assert!(out.limiter_engaged, "limiter did not engage on overshoot");
        let ceiling = 10.0f64.powf(limiter::CEILING_DBTP / 20.0);
        assert!(
            limiter::true_peak(&out.output) <= ceiling * 1.001,
            "true peak above ceiling"
        );
    }

    #[test]
    fn preview_latency_under_20_ms() {
        let mut engine = VocalEngine::new(RATE, QualityProfile::Preview).unwrap();
        let params = VocalParams {
            pitch_semitones: 3.0,
            air_percent: 30,
            tract_mm: 150.0,
        };
        let block = ts::sine(220.0, 0.5, 128, RATE);
        let mut fed = 0usize;
        let mut got = 0usize;
        for _ in 0..400 {
            let out = engine.process(&block, params).unwrap();
            fed += block.len();
            got += out.len();
            if fed >= 9600 {
                // 10 ms of stream; lag must stay under 20 ms (960 samples).
                assert!(
                    fed - got < 960,
                    "preview lag {} samples ({:.1} ms)",
                    fed - got,
                    (fed - got) as f64 / 48.0
                );
            }
        }
        let tail = engine.flush().unwrap();
        got += tail.len();
        assert_eq!(got, fed, "stream total != input total");
    }

    #[test]
    fn streaming_matches_offline_render() {
        let sig = vowel();
        let params = VocalParams {
            air_percent: -60,
            tract_mm: 150.0,
            ..VocalParams::neutral()
        };
        // Reference: the same engine fed in one bulk call (raw output —
        // `render` adds the offline true-peak guard, which the streaming
        // path intentionally lacks; see the limiter module docs).
        let mut ref_engine = VocalEngine::new(RATE, QualityProfile::Preview).unwrap();
        let mut offline = Vec::new();
        offline.extend(ref_engine.process(&sig, params).unwrap());
        offline.extend(ref_engine.flush().unwrap());

        let mut engine = VocalEngine::new(RATE, QualityProfile::Preview).unwrap();
        let mut streamed = Vec::new();
        let mut pos = 0;
        while pos < sig.len() {
            let end = (pos + 173).min(sig.len());
            streamed.extend(engine.process(&sig[pos..end], params).unwrap());
            pos = end;
        }
        streamed.extend(engine.flush().unwrap());
        assert_eq!(streamed.len(), offline.len());
        let max_diff = streamed
            .iter()
            .zip(&offline)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(
            max_diff < 1e-9,
            "streaming vs offline diverged by {max_diff:e}"
        );
    }

    #[test]
    fn render_profile_shape_works_too() {
        let params = VocalParams {
            pitch_semitones: 3.0,
            ..VocalParams::neutral()
        };
        let out = VocalEngine::render(&stack(), RATE, params, QualityProfile::Render).unwrap();
        assert_eq!(out.output.len(), stack().len());
        let expected = 220.0 * 2f64.powf(3.0 / 12.0);
        let got = f0_near(&out.output, expected);
        assert!(
            (got - expected).abs() < expected * 0.005,
            "render profile f0 {got:.2} vs {expected:.2}"
        );
    }

    #[test]
    fn render_interleaved_keeps_channels_separate() {
        let frames = 12_000;
        let left = ts::harmonic_stack(220.0, 8, 0.5, frames, RATE);
        let mut interleaved = Vec::with_capacity(frames * 2);
        for l in &left {
            interleaved.push(*l);
            interleaved.push(0.0);
        }
        let params = VocalParams {
            pitch_semitones: 2.0,
            ..VocalParams::neutral()
        };
        let out =
            VocalEngine::render_interleaved(&interleaved, 2, RATE, params, QualityProfile::Preview)
                .unwrap();
        assert_eq!(out.len(), interleaved.len());
        let right_max = out[1..]
            .iter()
            .step_by(2)
            .fold(0.0f32, |m, v| m.max(v.abs()));
        assert!(right_max < 1e-4, "right channel leaked {right_max}");
        let left_max = out.chunks(2).map(|c| c[0].abs()).fold(0.0f32, f32::max);
        assert!(left_max > 0.1, "left channel vanished");
    }

    #[test]
    fn non_finite_input_rejected() {
        let mut engine = VocalEngine::new(RATE, QualityProfile::Preview).unwrap();
        let bad = vec![0.0f32, f32::NAN, 0.5];
        let err = engine.process(&bad, VocalParams::neutral()).unwrap_err();
        assert!(matches!(err, EngineError::NonFiniteInput(1)), "got {err:?}");
    }

    #[test]
    fn empty_input_stream_is_clean() {
        let mut engine = VocalEngine::new(RATE, QualityProfile::Preview).unwrap();
        assert!(engine
            .process(&[], VocalParams::neutral())
            .unwrap()
            .is_empty());
        assert!(engine.flush().unwrap().is_empty());
        let out = VocalEngine::render(&[], RATE, VocalParams::neutral(), QualityProfile::Preview)
            .unwrap();
        assert!(out.output.is_empty());
    }
}
