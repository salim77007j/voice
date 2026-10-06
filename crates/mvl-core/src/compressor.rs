//! Broadband feed-forward compressor (Phase 8.4): the second V2 rack
//! module — a real dynamics stage wired 1:1 to the UI, with honest gain
//! reduction metering.
//!
//! Topology (the classic channel compressor, all in `f64`):
//!
//! ```text
//! in ─▶ |x| ─▶ dB ─▶ gain computer (static curve) ─▶ −GR target
//!                                                      │ one-pole ballistics
//!                                                      ▼
//! in ────────────────────────▶ ×10^((−GR + makeup)/20) ─▶ mix ─▶ out
//! ```
//!
//! Decisions worth recording:
//!
//! * **Log-domain decoupled smoothing.** The rectified level is converted
//!   to dB and the gain reduction is smoothed in the dB domain by a
//!   one-pole per direction (attack when the target deepens, release when
//!   it recovers). This is the standard program compressor of
//!   Reiss & McPherson (§6.4 "DAFX") — cheaper than a branchless
//!   linear-domain peak follower and analytically clean: with
//!   `coef = exp(−1/(τ·fs))` the envelope is an exact e-folding
//!   exponential, so "63 % of the step after τ" is a testable identity,
//!   not an approximation.
//! * **Soft knee in the log domain** (Zölzer's quadratic blend over a
//!   `knee_db`-wide window centred on the threshold). Knee 0 = hard
//!   knee. The formula is NaN-free over the whole documented range —
//!   unlike the shelf-slope form rejected in Phase 8.3, no guard is
//!   needed anywhere here.
//! * **Peak detector** (sample-absolute), not RMS: predictable on
//!   transients, deterministic for tests, and it makes the steady-state
//!   acceptance check (measured GR == static curve value) exact for
//!   constant-amplitude sine drives.
//! * **Manual makeup** (−24…+24 dB) on the wet path, plus a linear
//!   **mix** (0–100 %) for parallel ("New York") compression. Both are
//!   plain multiplications after the ballistics — no feedback paths, no
//!   allocation, no locks; the processor state is three `f64`s.
//! * **Bit-exact bypass** (invariant #1 extended): the engine skips this
//!   module entirely unless [`CompressorParams::is_active`] holds, and
//!   [`CompressorProcessor::process`] also refuses to touch the buffer
//!   when the params are inert — the belt and the braces. A −0.0 sentinel
//!   survives an inactive module (covered by test).
//!
//! Real-time contract: ballistic coefficients are redesigned lazily, only
//! when `(attack_ms, release_ms, rate)` changes; everything else is
//! arithmetic on the incoming params. `process` returns the deepest gain
//! reduction of the call (dB, ≤ 0) for block-rate metering.

/// Threshold range in dBFS (detector is peak-absolute).
pub const COMP_MIN_THRESHOLD_DB: f32 = -60.0;
pub const COMP_MAX_THRESHOLD_DB: f32 = 0.0;

/// Ratio range (`1.0` = identity — no compression).
pub const COMP_MIN_RATIO: f32 = 1.0;
pub const COMP_MAX_RATIO: f32 = 20.0;

/// Attack / release ranges in milliseconds (e-folding time constants).
pub const COMP_MIN_ATTACK_MS: f32 = 0.1;
pub const COMP_MAX_ATTACK_MS: f32 = 200.0;
pub const COMP_MIN_RELEASE_MS: f32 = 5.0;
pub const COMP_MAX_RELEASE_MS: f32 = 2_000.0;

/// Soft-knee width range in dB (0 = hard knee).
pub const COMP_MAX_KNEE_DB: f32 = 24.0;

/// Makeup gain range in dB (applied on the wet path).
pub const COMP_MAX_MAKEUP_DB: f32 = 24.0;

/// Mix (dry/wet) range in percent; 0 % = dry (inert), 100 % = full wet.
pub const COMP_MAX_MIX_PERCENT: f32 = 100.0;

/// Detector floor in dBFS: below this the log domain saturates. Keeps
/// `log10(0)` (silence, dropouts) well away from the math.
const DETECTOR_FLOOR_DB: f64 = -120.0;

/// `log2(10) / 20` — turns `exp2(x·k)` into the dB-domain `10^(x/20)`.
const LOG2_10_OVER_20: f64 = std::f64::consts::LOG2_10 / 20.0;

/// Gain-reduction clamp in dB (deep-compression guard; also the meter
/// full scale). `(0 − thr)·(1 − 1/r)` at the extremes stays below this.
const GR_CLAMP_DB: f64 = -80.0;

/// One compressor's parameters, as published to the DSP engine. Plain
/// `Copy`, travelling the same parameter channel as the rest of
/// [`crate::params::VocalParams`] — never the audio callback.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompressorParams {
    /// Master bypass. `false` = bit-exact passthrough regardless of the
    /// other fields.
    pub enabled: bool,
    /// Threshold in dBFS (peak detector domain), −60…0.
    pub threshold_db: f32,
    /// Compression ratio, 1…20 (`1.0` = no compression).
    pub ratio: f32,
    /// Attack time constant in ms (e-folding), 0.1…200.
    pub attack_ms: f32,
    /// Release time constant in ms (e-folding), 5…2000.
    pub release_ms: f32,
    /// Soft-knee width in dB, 0…24 (0 = hard knee).
    pub knee_db: f32,
    /// Makeup gain in dB, −24…+24 (wet path).
    pub makeup_db: f32,
    /// Dry/wet mix in percent, 0…100 (0 = dry only, 100 = full wet).
    pub mix_percent: f32,
}

impl Default for CompressorParams {
    fn default() -> Self {
        Self {
            enabled: false,
            threshold_db: -20.0,
            ratio: 3.0,
            attack_ms: 10.0,
            release_ms: 120.0,
            knee_db: 6.0,
            makeup_db: 0.0,
            mix_percent: 100.0,
        }
    }
}

impl CompressorParams {
    /// The identity parameter set: acoustically inert by construction.
    pub const fn neutral() -> Self {
        Self {
            enabled: false,
            threshold_db: -20.0,
            ratio: 1.0,
            attack_ms: 10.0,
            release_ms: 120.0,
            knee_db: 6.0,
            makeup_db: 0.0,
            mix_percent: 100.0,
        }
    }

    /// True when the module would process audio (signal-independent):
    /// enabled, the wet path actually mixes in, and something does work
    /// (a ratio above 1:1, or a non-zero makeup on the wet path). The
    /// engine uses this for the bit-exact bypass decision — a module
    /// with `mix == 0` or `ratio == 1`/`makeup == 0` only is skipped so
    /// the passthrough stays bit-exact.
    pub fn is_active(&self) -> bool {
        self.enabled && self.mix_percent > 0.0 && (self.ratio > 1.0 || self.makeup_db != 0.0)
    }

    /// Return a copy clamped into the documented ranges, with
    /// non-finite fields falling back to the neutral values — the engine
    /// must never see NaN in the detector or ballistics.
    #[must_use]
    pub fn sanitized(&self) -> Self {
        let fin_or = |v: f32, fb: f32| if v.is_finite() { v } else { fb };
        Self {
            enabled: self.enabled,
            threshold_db: fin_or(self.threshold_db, Self::neutral().threshold_db)
                .clamp(COMP_MIN_THRESHOLD_DB, COMP_MAX_THRESHOLD_DB),
            ratio: fin_or(self.ratio, Self::neutral().ratio).clamp(COMP_MIN_RATIO, COMP_MAX_RATIO),
            attack_ms: fin_or(self.attack_ms, Self::neutral().attack_ms)
                .clamp(COMP_MIN_ATTACK_MS, COMP_MAX_ATTACK_MS),
            release_ms: fin_or(self.release_ms, Self::neutral().release_ms)
                .clamp(COMP_MIN_RELEASE_MS, COMP_MAX_RELEASE_MS),
            knee_db: fin_or(self.knee_db, Self::neutral().knee_db).clamp(0.0, COMP_MAX_KNEE_DB),
            makeup_db: fin_or(self.makeup_db, Self::neutral().makeup_db)
                .clamp(-COMP_MAX_MAKEUP_DB, COMP_MAX_MAKEUP_DB),
            mix_percent: fin_or(self.mix_percent, Self::neutral().mix_percent)
                .clamp(0.0, COMP_MAX_MIX_PERCENT),
        }
    }
}

/// Built-in starting points (display order == the UI ComboBox order).
/// Values follow the classic vocal-channel conventions: dBFS peak
/// thresholds, e-folding ballistics, percentage mix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompPreset {
    /// Subtle glue: slow attack keeps consonants, knee wide, no makeup.
    Gentle,
    /// The workhorse vocal setting: 3.5:1 around −20 dBFS, a touch of
    /// makeup to level the take.
    VocalControl,
    /// Hard-knee peak catcher: fast attack catches plosives and clicks.
    PeakTamer,
    /// Denser, louder, always-on: low threshold, moderate ratio, makeup.
    Broadcast,
    /// Parallel compression: crushed wet path blended at 30 % under the
    /// dry signal — density without losing the peaks.
    NyParallel,
}

pub const COMP_PRESETS: [CompPreset; 5] = [
    CompPreset::Gentle,
    CompPreset::VocalControl,
    CompPreset::PeakTamer,
    CompPreset::Broadcast,
    CompPreset::NyParallel,
];

impl CompPreset {
    /// The full parameter set of this preset (enabled by definition).
    pub fn params(self) -> CompressorParams {
        let (thr, ratio, att, rel, knee, makeup, mix) = match self {
            Self::Gentle => (-18.0, 2.0, 15.0, 250.0, 12.0, 0.0, 100.0),
            Self::VocalControl => (-20.0, 3.5, 8.0, 150.0, 6.0, 2.5, 100.0),
            Self::PeakTamer => (-8.0, 6.0, 0.5, 80.0, 0.0, 0.0, 100.0),
            Self::Broadcast => (-26.0, 4.0, 2.0, 120.0, 8.0, 4.0, 100.0),
            Self::NyParallel => (-30.0, 8.0, 20.0, 200.0, 12.0, 0.0, 30.0),
        };
        CompressorParams {
            enabled: true,
            threshold_db: thr,
            ratio,
            attack_ms: att,
            release_ms: rel,
            knee_db: knee,
            makeup_db: makeup,
            mix_percent: mix,
        }
    }
}

/// The static gain computer: instantaneous gain reduction (dB, ≤ 0) the
/// curve prescribes for a detector level of `x_db`, given threshold,
/// ratio and knee width. Pure — shared by the processor, the UI transfer
/// curve, and the tests.
///
/// Piecewise (Zölzer, *DAFX* 2nd ed. §4.3.2):
/// * `x ≤ T − W/2` → 0 (below the knee, no reduction)
/// * `T − W/2 < x < T + W/2` → quadratic blend
///   `((1/R − 1)·(x − T + W/2)²) / (2W)`
/// * `x ≥ T + W/2` → `(x − T)·(1 − 1/R)`
///
/// `R == 1` is the identity everywhere (the knee term vanishes with it),
/// and `W == 0` degenerates to the hard knee — both NaN-free.
#[must_use]
pub fn static_gain_db(threshold_db: f64, ratio: f64, knee_db: f64, x_db: f64) -> f64 {
    if ratio <= 1.0 {
        return 0.0;
    }
    let slope = 1.0 - 1.0 / ratio;
    let w = knee_db.max(0.0);
    let half = w / 2.0;
    if x_db <= threshold_db - half {
        0.0
    } else if x_db >= threshold_db + half {
        -((x_db - threshold_db) * slope)
    } else if w == 0.0 {
        // knee width 0 with floating-point comparison slack: hard knee.
        -((x_db - threshold_db) * slope)
    } else {
        let d = x_db - threshold_db + half;
        // (1/R − 1) < 0 for R > 1, so the gain is negative (a cut) and
        // continuous with both neighbours — no extra negation, the sign
        // lives in (1/R − 1).
        (1.0 / ratio - 1.0) * d * d / (2.0 * w)
    }
}

/// The compressor processor: ballistic envelope + gain application for
/// one channel. One instance per engine (per channel), fed in stream
/// order; state is three `f64`s and the cached ballistics — no
/// allocation, no locks, RT-safe.
#[derive(Debug, Clone)]
pub struct CompressorProcessor {
    rate: u32,
    /// Smoothed gain reduction, dB (≤ 0). 0 = no reduction.
    gr_db: f64,
    /// Cached one-pole coefficients for the current (attack, release,
    /// rate) triple — redesigned lazily.
    attack_coef: f64,
    release_coef: f64,
    cache_key: (u32, u32), // attack_ms / release_ms as bits + rate
    last_params: Option<CompressorParams>,
}

impl CompressorProcessor {
    /// Create a processor for `rate` (0 is clamped by the engine's own
    /// guard; a zero rate here would only disable time constants).
    pub fn new(rate: u32) -> Self {
        Self {
            rate,
            gr_db: 0.0,
            attack_coef: 0.0,
            release_coef: 0.0,
            cache_key: (0, 0),
            last_params: None,
        }
    }

    /// Forget the envelope (master bypass re-engage, stream restart).
    /// Coefficients stay cached — only the dynamic state clears.
    pub fn reset(&mut self) {
        self.gr_db = 0.0;
        self.last_params = None;
    }

    /// Change the session rate (redesigns ballistics on next use).
    pub fn set_rate(&mut self, rate: u32) {
        if self.rate != rate {
            self.rate = rate;
            self.cache_key = (0, 0);
        }
    }

    /// Current smoothed gain reduction in dB (for tests and metering).
    pub fn gain_reduction_db(&self) -> f64 {
        self.gr_db
    }

    /// Process `io` in place under `params`. Returns the deepest gain
    /// reduction applied during this call (dB, ≤ 0) — the block-rate
    /// meter value.
    ///
    /// Inert params leave the buffer **bit-exact untouched** (and return
    /// 0.0): the engine's bypass contract, enforced at the module level
    /// too.
    pub fn process(&mut self, params: &CompressorParams, io: &mut [f32]) -> f32 {
        if !params.is_active() {
            return 0.0;
        }
        let params = params.sanitized();
        self.redesign(&params);

        let thr = f64::from(params.threshold_db);
        let ratio = f64::from(params.ratio);
        let knee = f64::from(params.knee_db);
        let makeup_lin = 10.0f64.powf(f64::from(params.makeup_db) / 20.0);
        let mix = f64::from(params.mix_percent) / 100.0;
        let mut deepest = 0.0f64;

        for s in io.iter_mut() {
            let x = f64::from(*s);
            // Detector: peak-absolute → dB (floored to keep log finite).
            let level = x.abs();
            let x_db = (if level > 0.0 {
                20.0 * level.log10()
            } else {
                DETECTOR_FLOOR_DB
            })
            .max(DETECTOR_FLOOR_DB);

            // Static curve → target GR, then one-pole ballistics in the
            // dB domain (attack deepens, release recovers).
            let target = (static_gain_db(thr, ratio, knee, x_db)).max(GR_CLAMP_DB);
            let coef = if target < self.gr_db {
                self.attack_coef
            } else {
                self.release_coef
            };
            self.gr_db = coef * self.gr_db + (1.0 - coef) * target;
            deepest = deepest.min(self.gr_db);

            // Apply: wet = in · 10^((GR + makeup)/20); out = dry + mix·(wet − dry).
            // exp2(x·log2(10)/20) == 10^(x/20), one instruction cheaper
            // than powf and exact to the last ulp over this range.
            let gain = f64::exp2(self.gr_db * LOG2_10_OVER_20) * makeup_lin;
            let wet = x * gain;
            *s = (x + mix * (wet - x)) as f32;
        }
        deepest as f32
    }

    /// One-pole coefficient for an e-folding time constant `τ` (ms):
    /// `exp(−1/(τ·fs))` — after `τ` the envelope has covered `1 − 1/e`
    /// of the step. Cached on the (attack, release, rate) triple.
    fn redesign(&mut self, p: &CompressorParams) {
        let key = (f32::to_bits(p.attack_ms), f32::to_bits(p.release_ms));
        // `last_params` equality covers threshold/knee/makeup/mix changes
        // (no redesign needed); the key covers the ballistics triple —
        // `set_rate` invalidates it to (0, 0), forcing a redesign.
        if self.last_params == Some(*p) && self.cache_key == key {
            return;
        }
        let ms_to_coef = |ms: f32| {
            let tau = f64::from(ms) / 1000.0;
            if tau <= 0.0 || self.rate == 0 {
                0.0
            } else {
                (-1.0 / (tau * f64::from(self.rate))).exp()
            }
        };
        let a = ms_to_coef(p.attack_ms);
        let r = ms_to_coef(p.release_ms);
        self.attack_coef = a;
        self.release_coef = r;
        self.cache_key = key;
        self.last_params = Some(*p);
    }
}

/// Static transfer curve for the UI: `points` (in_db, out_db) pairs on a
/// linear dB grid from −80 to 0 dBFS. `out = in + static_gain_db(…)`.
/// The controller draws the unity diagonal plus this curve with the same
/// math the audio path uses.
#[must_use]
pub fn transfer_pairs_db(params: &CompressorParams, points: usize) -> Vec<(f64, f64)> {
    let p = params.sanitized();
    let n = points.max(2);
    (0..n)
        .map(|i| {
            let x = -80.0 + 80.0 * i as f64 / (n - 1) as f64;
            let g = static_gain_db(
                f64::from(p.threshold_db),
                f64::from(p.ratio),
                f64::from(p.knee_db),
                x,
            );
            (x, x + g)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;

    fn rms(x: &[f32]) -> f64 {
        (x.iter().map(|v| f64::from(*v) * f64::from(*v)).sum::<f64>() / x.len() as f64).sqrt()
    }

    fn db(x: f64) -> f64 {
        20.0 * x.log10()
    }

    fn sine(seconds: f64, amp: f32, rate: u32) -> Vec<f32> {
        let n = (seconds * f64::from(rate)) as usize;
        (0..n)
            .map(|i| {
                (2.0 * std::f64::consts::PI * 1_000.0 * i as f64 / f64::from(rate)).sin()
                    * f64::from(amp)
            })
            .map(|v| v as f32)
            .collect()
    }

    // ---- static gain computer -----------------------------------------

    /// Hard-knee arithmetic checked against hand math at the classic
    /// points: below / at / above threshold.
    #[test]
    fn static_gain_hard_knee_matches_hand_math() {
        let thr = -20.0;
        // below: no reduction
        assert_eq!(static_gain_db(thr, 4.0, 0.0, -30.0), 0.0);
        assert_eq!(static_gain_db(thr, 4.0, 0.0, thr), 0.0);
        // above: (x − T)·(1 − 1/R)
        let over = static_gain_db(thr, 4.0, 0.0, -8.0);
        assert!((over - (-9.0)).abs() < 1e-12, "{over}");
        // 10 dB over at 20:1 → 9.5 dB GR
        let deep = static_gain_db(thr, 20.0, 0.0, -10.0);
        assert!((deep - (-9.5)).abs() < 1e-12, "{deep}");
        // ratio 1 → identity everywhere, even far above threshold
        assert_eq!(static_gain_db(thr, 1.0, 6.0, 0.0), 0.0);
    }

    /// Soft-knee quadratic blend verified at the knee edges (must match
    /// the adjacent segments continuously) and the centre (slope halved
    /// there — the defining property of the quadratic blend).
    #[test]
    fn soft_knee_is_continuous_and_half_slope_at_centre() {
        let thr = -20.0f64;
        let w = 8.0f64;
        let r = 4.0f64;
        // at the knee edges the blend must equal the neighbouring segments
        let lo = static_gain_db(thr, r, w, thr - w / 2.0);
        assert_eq!(lo, 0.0, "lower knee edge joins the no-reduction region");
        let hi = static_gain_db(thr, r, w, thr + w / 2.0);
        let above = -(((thr + w / 2.0) - thr) * (1.0 - 1.0 / r));
        assert!((hi - above).abs() < 1e-12, "{hi} vs {above}");
        // centre value from the closed form: d = W/2 → GR = (1/R − 1)·W/8
        let mid = static_gain_db(thr, r, w, thr);
        let expected = (1.0 / r - 1.0) * (w / 2.0) * (w / 2.0) / (2.0 * w);
        assert!((mid - expected).abs() < 1e-12, "{mid}");
        // and it lies strictly below the lower-edge gain: the knee region
        // genuinely reduces (gain < 0 dB) and deepens toward the centre.
        let q_lo = static_gain_db(thr, r, w, thr - w / 4.0);
        assert!(
            q_lo < lo,
            "knee region reduces strictly monotonically ({q_lo} vs {lo})"
        );
    }

    // ---- acceptance: steady-state GR == static curve (±0.5 dB) ---------

    /// THE acceptance criterion (roadmap §8.7): the measured steady-state
    /// gain reduction equals the analytic static curve within ±0.5 dB —
    /// hard and soft knees, several ratios and levels.
    ///
    /// The stimulus is a **constant-level** signal, not a sine: the peak
    /// detector is sample-absolute, so a rectified sine sweeps the whole
    /// dB trajectory twice per cycle and the smoothed envelope settles at
    /// a weighted average *of the curve* — a fine compressor behaviour,
    /// but the wrong probe for an exact static-curve identity. Constant
    /// level pins the detector to one dB point, making the identity
    /// exact; the envelope dynamics are covered by the ballistics tests
    /// below.
    #[test]
    fn steady_state_gr_matches_analytic_curve() {
        let cases = [
            // (threshold, ratio, knee, drive dBFS)
            (-20.0f32, 4.0f32, 0.0f32, -6.0f32),
            (-20.0, 4.0, 6.0, -6.0),
            (-30.0, 2.0, 12.0, -10.0),
            (-12.0, 8.0, 0.0, -1.0),
            (-40.0, 20.0, 4.0, -6.0),
            (-18.0, 3.5, 6.0, -0.5),
            (-20.0, 4.0, 8.0, -17.0), // drive strictly inside the knee
        ];
        for (thr, ratio, knee, drive_db) in cases {
            let p = CompressorParams {
                enabled: true,
                threshold_db: thr,
                ratio,
                attack_ms: 5.0,
                release_ms: 80.0,
                knee_db: knee,
                makeup_db: 0.0,
                mix_percent: 100.0,
            };
            // 1 s at constant amplitude: the envelope settles to the
            // static target after ~5 attack constants and stays. The RMS
            // is measured on the settled tail only — the short attack
            // ramp at the head would otherwise skew a deep compression
            // (its energy dwarfs the tiny steady output).
            let amp = 10.0f32.powf(drive_db / 20.0);
            let input = vec![amp; RATE as usize];
            let mut cp = CompressorProcessor::new(RATE);
            let mut out = input.clone();
            cp.process(&p, &mut out);

            let tail = RATE as usize / 2;
            let gr_measured = db(rms(&input[tail..])) - db(rms(&out[tail..]));
            let x_db = f64::from(drive_db); // detector level == drive level
            let gr_analytic =
                -static_gain_db(f64::from(thr), f64::from(ratio), f64::from(knee), x_db);
            assert!(
                (gr_measured - gr_analytic).abs() <= 0.5,
                "thr {thr} r {ratio} knee {knee}: measured {gr_measured:.3} dB, analytic {gr_analytic:.3} dB"
            );
        }
    }

    // ---- ballistics ----------------------------------------------------

    /// One-pole identity: after one attack time constant the envelope has
    /// covered 1 − 1/e ≈ 63.2 % of the step (documented convention).
    ///
    /// The stimulus is a constant *level* step, not a gated sine: the
    /// peak detector is sample-absolute, so a sine's rectified trajectory
    /// wiggles the target twice per cycle and the asymmetric ballistics
    /// ratchet (fast attack, slow release) — real compressor behaviour,
    /// but it would bury the exact one-pole identity this test pins. A
    /// constant level pins the detector, and `out/in` per sample is then
    /// the exact instantaneous gain — no windowing needed.
    #[test]
    fn attack_covers_63_percent_after_tau() {
        let att_ms = 20.0f32;
        let p = CompressorParams {
            enabled: true,
            threshold_db: -30.0,
            ratio: 8.0,
            attack_ms: att_ms,
            release_ms: 400.0,
            knee_db: 0.0,
            makeup_db: 0.0,
            mix_percent: 100.0,
        };
        // quiet constant (below threshold, GR 0), then a loud constant
        let mut input = vec![0.01f32; (0.1 * f64::from(RATE)) as usize];
        input.extend(vec![0.5f32; (0.4 * f64::from(RATE)) as usize]);
        let mut cp = CompressorProcessor::new(RATE);
        let mut out = input.clone();
        cp.process(&p, &mut out);

        let t0 = (0.1 * f64::from(RATE)) as usize;
        let tau_n = (f64::from(att_ms) / 1000.0 * f64::from(RATE)).round() as usize;
        let gain_at = |n: usize| -> f64 { f64::from(out[n]) / f64::from(input[n]) };
        let gr_tau_db = -db(gain_at(t0 + tau_n));
        let gr_final_db = -db(gain_at(out.len() - 1));
        // gr(τ) / gr(∞) == 1 − 1/e in the dB domain the one-pole runs in
        let frac = gr_tau_db / gr_final_db;
        assert!(
            (frac - (1.0 - std::f64::consts::E.recip())).abs() < 0.02,
            "covered fraction {frac:.4} at τ (gr_tau {gr_tau_db:.2} dB of {gr_final_db:.2} dB)"
        );
    }

    /// Symmetric check on the release leg (same constant-level reasoning).
    #[test]
    fn release_covers_63_percent_after_tau() {
        let rel_ms = 200.0f32;
        let p = CompressorParams {
            enabled: true,
            threshold_db: -30.0,
            ratio: 8.0,
            attack_ms: 2.0,
            release_ms: rel_ms,
            knee_db: 0.0,
            makeup_db: 0.0,
            mix_percent: 100.0,
        };
        // loud constant, then quiet constant: GR must recover toward 0
        let mut input = vec![0.5f32; (0.3 * f64::from(RATE)) as usize];
        input.extend(vec![0.01f32; (0.8 * f64::from(RATE)) as usize]);
        let mut cp = CompressorProcessor::new(RATE);
        let mut out = input.clone();
        cp.process(&p, &mut out);

        let t1 = (0.3 * f64::from(RATE)) as usize;
        let tau_n = (f64::from(rel_ms) / 1000.0 * f64::from(RATE)).round() as usize;
        let gain_at = |n: usize| -> f64 { f64::from(out[n]) / f64::from(input[n]) };
        let gr_before_db = -db(gain_at(t1 - 2));
        let gr_tau_db = -db(gain_at(t1 + tau_n));
        // recovery in the dB domain: gr(τ) = gr_before·1/e
        let frac = (gr_before_db - gr_tau_db) / gr_before_db;
        assert!(
            (frac - (1.0 - std::f64::consts::E.recip())).abs() < 0.02,
            "recovered fraction {frac:.4} at τ (before {gr_before_db:.2} dB, at τ {gr_tau_db:.2} dB)"
        );
    }

    /// Rate change must redesign the ballistics: the same 20 ms attack
    /// reaches 63 % at 20 ms on every rate — the τ identity is rate-aware.
    #[test]
    fn rate_change_redesigns_ballistics() {
        let p = CompressorParams {
            enabled: true,
            threshold_db: -30.0,
            ratio: 8.0,
            attack_ms: 20.0,
            release_ms: 400.0,
            knee_db: 0.0,
            makeup_db: 0.0,
            mix_percent: 100.0,
        };
        let frac_at = |rate: u32| {
            let mut input = vec![0.01f32; (0.1 * f64::from(rate)) as usize];
            input.extend(vec![0.5f32; (0.4 * f64::from(rate)) as usize]);
            let mut cp = CompressorProcessor::new(rate);
            let mut out = input.clone();
            cp.process(&p, &mut out);
            let t0 = (0.1 * f64::from(rate)) as usize;
            let tau_n = (0.02 * f64::from(rate)).round() as usize;
            let gain_at = |n: usize| -> f64 { f64::from(out[n]) / f64::from(input[n]) };
            let gr_tau_db = -db(gain_at(t0 + tau_n));
            let gr_final_db = -db(gain_at(out.len() - 1));
            gr_tau_db / gr_final_db
        };
        for rate in [44_100u32, 48_000, 96_000] {
            let frac = frac_at(rate);
            assert!(
                (frac - (1.0 - std::f64::consts::E.recip())).abs() < 0.02,
                "rate {rate}: fraction {frac:.4}"
            );
        }
    }

    // ---- bit-exactness and mix ------------------------------------------

    /// Inactive / disabled / fully-neutral processors are bit-exact,
    /// including the −0.0 sentinel and denormal-ish quiet samples.
    #[test]
    fn inactive_is_bit_exact() {
        let variants = [
            CompressorParams::neutral(),
            CompressorParams {
                enabled: false,
                ..CompressorParams::default()
            },
            // enabled but nothing to do: ratio 1, no makeup, full wet
            CompressorParams {
                enabled: true,
                ratio: 1.0,
                ..CompressorParams::default()
            },
            // enabled, real compression, but mix 0 % → dry only
            CompressorParams {
                enabled: true,
                ratio: 6.0,
                mix_percent: 0.0,
                ..CompressorParams::default()
            },
        ];
        let mut input = sine(0.05, 0.5, RATE);
        input[10] = -0.0; // sentinel
        input[11] = 1e-30; // denormal-scale
        input[12] = 0.0;
        for (i, p) in variants.iter().enumerate() {
            assert!(!p.is_active(), "variant {i} should be inert");
            let mut cp = CompressorProcessor::new(RATE);
            let mut out = input.clone();
            let gr = cp.process(p, &mut out);
            assert_eq!(gr, 0.0);
            assert_eq!(out, input, "variant {i} must be bit-exact");
        }
    }

    /// Mix crossfades deterministically: 50 % == dry + ½(wet − dry),
    /// where `wet` is the 100 % run from an identical reset state.
    #[test]
    fn mix_crossfades_deterministically() {
        let base = CompressorParams {
            enabled: true,
            threshold_db: -20.0,
            ratio: 4.0,
            attack_ms: 5.0,
            release_ms: 100.0,
            knee_db: 4.0,
            makeup_db: 3.0,
            mix_percent: 100.0,
        };
        let input = sine(0.2, 0.4, RATE);
        let mut wet = input.clone();
        CompressorProcessor::new(RATE).process(&base, &mut wet);
        let mut half = input.clone();
        CompressorProcessor::new(RATE).process(
            &CompressorParams {
                mix_percent: 50.0,
                ..base
            },
            &mut half,
        );
        for (i, (h, x)) in half.iter().zip(&input).enumerate() {
            let expect = f64::from(*x) + 0.5 * (f64::from(wet[i]) - f64::from(*x));
            assert!((f64::from(*h) - expect).abs() < 1e-6, "sample {i}");
        }
    }

    /// Makeup applies to the wet path exactly: below threshold (GR 0),
    /// wet = in · 10^(makeup/20) at 100 % mix.
    #[test]
    fn makeup_is_exact_below_threshold() {
        // knee 6 dB around −4 dBFS spans −7…−1: the drive's peak (−12)
        // stays under the whole knee → zero GR, pure makeup.
        let p = CompressorParams {
            enabled: true,
            threshold_db: -4.0,
            ratio: 2.0,
            makeup_db: 6.0,
            ..CompressorParams::default()
        };
        let input = sine(0.1, 0.25, RATE); // peak −12 dBFS
        let mut out = input.clone();
        CompressorProcessor::new(RATE).process(&p, &mut out);
        let g = db(rms(&out)) - db(rms(&input));
        assert!((g - 6.0).abs() < 0.01, "makeup 6 vs measured {g:.4}");
    }

    // ---- robustness ------------------------------------------------------

    /// The full documented parameter range stays finite, bounded and
    /// panic-free on noise, impulses, DC and silence.
    #[test]
    fn full_range_stays_finite_and_bounded() {
        let mut rng_state = 0x1234_5678_u32;
        let mut noise = Vec::with_capacity(2_048);
        for _ in 0..2_048 {
            rng_state = rng_state
                .wrapping_mul(1_664_525)
                .wrapping_add(1_013_904_223);
            noise.push(((rng_state >> 9) as f32 / 8_388_608.0) - 1.0);
        }
        let impulse = {
            let mut v = vec![0.0f32; 1_024];
            v[0] = 1.0;
            v[512] = -1.0;
            v
        };
        let dc = vec![0.75f32; 1_024];
        let silence = vec![0.0f32; 1_024];
        for thr in [COMP_MIN_THRESHOLD_DB, -20.0, COMP_MAX_THRESHOLD_DB] {
            for ratio in [COMP_MIN_RATIO, 1.35, COMP_MAX_RATIO] {
                for att in [COMP_MIN_ATTACK_MS, 10.0, COMP_MAX_ATTACK_MS] {
                    for rel in [COMP_MIN_RELEASE_MS, 120.0, COMP_MAX_RELEASE_MS] {
                        for knee in [0.0, 24.0] {
                            for makeup in [-COMP_MAX_MAKEUP_DB, 0.0, COMP_MAX_MAKEUP_DB] {
                                for mix in [0.0, 37.0, COMP_MAX_MIX_PERCENT] {
                                    let p = CompressorParams {
                                        enabled: true,
                                        threshold_db: thr,
                                        ratio,
                                        attack_ms: att,
                                        release_ms: rel,
                                        knee_db: knee,
                                        makeup_db: makeup,
                                        mix_percent: mix,
                                    };
                                    let mut cp = CompressorProcessor::new(RATE);
                                    for src in [&noise, &impulse, &dc, &silence] {
                                        let mut buf = src.clone();
                                        let gr = cp.process(&p, &mut buf);
                                        assert!(buf.iter().all(|v| v.is_finite()));
                                        assert!(gr.is_finite());
                                        assert!((f64::from(gr) >= GR_CLAMP_DB - 1e-6));
                                        assert!(gr <= 0.0);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// `reset` clears the envelope: after a hard squeeze + reset, a quiet
    /// passage passes bit-exact again (GR 0 → gain exactly 1).
    #[test]
    fn reset_clears_envelope() {
        let p = CompressorParams {
            enabled: true,
            threshold_db: -40.0,
            ratio: 20.0,
            attack_ms: 1.0,
            release_ms: 500.0,
            knee_db: 0.0,
            makeup_db: 0.0,
            mix_percent: 100.0,
        };
        let loud = sine(0.05, 0.9, RATE);
        // −60 dBFS, far below the −40 dBFS threshold: zero GR even with
        // a fresh envelope, so the passage must be bit-exact.
        let quiet = sine(0.01, 0.001, RATE);
        let mut cp = CompressorProcessor::new(RATE);
        let mut squeezed = loud.clone();
        cp.process(&p, &mut squeezed);
        assert!(cp.gain_reduction_db() < -1.0, "must be compressing");
        cp.reset();
        let mut out = quiet.clone();
        cp.process(&p, &mut out);
        assert_eq!(
            out, quiet,
            "fresh envelope passes the quiet passage untouched"
        );
    }

    /// The UI transfer curve is the static curve on the documented grid.
    #[test]
    fn transfer_curve_matches_static_math() {
        let p = CompressorParams {
            enabled: true,
            threshold_db: -20.0,
            ratio: 4.0,
            knee_db: 6.0,
            ..CompressorParams::default()
        };
        let pts = transfer_pairs_db(&p, 81);
        assert_eq!(pts.len(), 81);
        let (first_in, first_out) = pts[0];
        assert!((first_in - (-80.0)).abs() < 1e-9);
        assert!(
            (first_out - first_in).abs() < 1e-9,
            "−80 dBFS is below any knee"
        );
        let (last_in, last_out) = pts[80];
        assert!((last_in - 0.0).abs() < 1e-9);
        let expect = last_in
            + static_gain_db(
                f64::from(p.threshold_db),
                f64::from(p.ratio),
                f64::from(p.knee_db),
                0.0,
            );
        assert!((last_out - expect).abs() < 1e-9);
        // the reduction (in − out, ≥ 0) deepens monotonically over the sweep
        let mut prev_red = 0.0;
        for (x, y) in &pts {
            let red = x - y;
            assert!(red >= -1e-9, "reduction never negative at {x}");
            assert!(
                red >= prev_red - 1e-9,
                "reduction deepens monotonically at {x}"
            );
            prev_red = red;
        }
    }
}
