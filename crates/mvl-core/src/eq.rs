//! Four-band parametric EQ (Phase 8.3): the first module of the V2
//! rack expansion — a real channel EQ, not a spectral toy.
//!
//! Bands are fixed by position (the classic vocal-channel layout):
//!
//! | # | kind         | musical job                          |
//! |---|--------------|--------------------------------------|
//! | 0 | low shelf    | rumble / body / mud control          |
//! | 1 | bell         | boxiness (200–500 Hz) surgery        |
//! | 2 | bell         | presence / harshness (1–5 kHz)       |
//! | 3 | high shelf   | air / dulling (8 kHz +)              |
//!
//! Each band exposes frequency (20 Hz – 20 kHz), Q (0.1 – 10) and gain
//! (±24 dB), plus a bypass toggle; the whole EQ has a master bypass.
//! Every control maps onto Robert Bristow-Johnson's Audio EQ Cookbook
//! biquad coefficients (`Audio-EQ-Cookbook.txt`, RBJ 2003) computed in
//! `f64`, with the shelf bands sharing the bell's `α = sin ω₀ / 2Q`
//! bandwidth convention so `Q` behaves consistently across all four
//! bands (higher Q = tighter knee, NaN-free across the full stated
//! range — the cookbook's shelf-slope `S` form would demand NaN checks
//! for `S > 1` at large boosts).
//!
//! Signal path: the engine applies the EQ **after** its existing
//! modules (air → formant → pitch → true-peak guard) and after the
//! bit-exact neutral bypass, so a disabled/flat EQ is a true bit-exact
//! passthrough (invariant #1 extended to the EQ) and an EQ-only session
//! never engages the STFT machinery.
//!
//! Real-time contract: coefficient redesign happens per `process` call
//! (block rate) and only when a band's triple actually changed; the
//! biquad state is two `f64` accumulators per band — no allocation, no
//! locks. Zipper noise under fast drags is accepted at block rate for
//! 8.3 (documented in the phase report; sub-block crossfade is the 8.4
//! refinement if listening tests ask for it).

/// Frequency range of every band (Hz).
pub const EQ_MIN_FREQ_HZ: f32 = 20.0;
/// Upper frequency bound (Hz).
pub const EQ_MAX_FREQ_HZ: f32 = 20_000.0;
/// Q range of every band.
pub const EQ_MIN_Q: f32 = 0.1;
/// Upper Q bound.
pub const EQ_MAX_Q: f32 = 10.0;
/// Gain range of every band (dB).
pub const EQ_MAX_GAIN_DB: f32 = 24.0;

/// Number of points the UI response curve is evaluated at (odd, so the
/// 1 kHz decade centre lands exactly on a point).
pub const EQ_CURVE_POINTS: usize = 129;

/// Band type, fixed by position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EqBandKind {
    /// Band 0 — low shelf.
    LowShelf,
    /// Bands 1 and 2 — peaking bell.
    Bell,
    /// Band 3 — high shelf.
    HighShelf,
}

/// The kind of band `i` (0..4).
#[must_use]
pub fn band_kind(i: usize) -> EqBandKind {
    match i {
        0 => EqBandKind::LowShelf,
        1 | 2 => EqBandKind::Bell,
        _ => EqBandKind::HighShelf,
    }
}

/// One EQ band's user parameters. The band's *kind* is not part of the
/// parameters — it is fixed by the band's position in [`EqParams`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EqBandParams {
    /// Centre / corner frequency in Hz, `20 ..= 20_000`.
    pub freq: f32,
    /// Resonance (bell) or knee sharpness (shelf), `0.1 ..= 10`.
    pub q: f32,
    /// Cut / boost in dB, `−24 ..= +24`.
    pub gain_db: f32,
    /// Per-band bypass: `false` skips the band entirely (bit-exact).
    pub enabled: bool,
}

impl EqBandParams {
    /// The band's rest state: neutral frequency (per-position default),
    /// Butterworth-ish Q, zero gain, engaged.
    pub const fn neutral(default_freq: f32) -> Self {
        Self {
            freq: default_freq,
            q: 0.707,
            gain_db: 0.0,
            enabled: true,
        }
    }

    /// True when the band is acoustically inert (bypassed or zero gain).
    /// A zero-gain band's RBJ coefficients are exactly the identity
    /// transfer function, so skipping it is both a CPU saving and the
    /// bit-exactness guarantee (no `−0.0 → +0.0` sign flips).
    #[must_use]
    pub fn is_inert(&self) -> bool {
        !self.enabled || self.gain_db == 0.0
    }

    /// Clamp into the documented ranges; non-finite values (which the UI
    /// cannot produce but hand-written code might) fall back to the
    /// neutral defaults.
    #[must_use]
    pub fn sanitized(&self, default_freq: f32) -> Self {
        let clamp_f = |v: f32, lo: f32, hi: f32, fallback: f32| {
            if v.is_finite() {
                v.clamp(lo, hi)
            } else {
                fallback
            }
        };
        Self {
            freq: clamp_f(self.freq, EQ_MIN_FREQ_HZ, EQ_MAX_FREQ_HZ, default_freq),
            q: clamp_f(self.q, EQ_MIN_Q, EQ_MAX_Q, 0.707),
            gain_db: clamp_f(self.gain_db, -EQ_MAX_GAIN_DB, EQ_MAX_GAIN_DB, 0.0),
            enabled: self.enabled,
        }
    }
}

/// The whole EQ's parameters: master enable + four positioned bands.
///
/// `Copy` by design: the struct travels the same lock-free-ish parameter
/// path as [`crate::VocalParams`] (channel + `Mutex`, never the audio
/// callback), and `VocalParams` embeds it whole.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EqParams {
    /// Master bypass: `false` skips the EQ entirely (bit-exact).
    pub enabled: bool,
    /// Band 0 — low shelf.
    pub low: EqBandParams,
    /// Band 1 — low-mid bell.
    pub low_mid: EqBandParams,
    /// Band 2 — high-mid bell.
    pub high_mid: EqBandParams,
    /// Band 3 — high shelf.
    pub high: EqBandParams,
}

/// Rest-state band frequencies (Hz) — vocal-channel anchors, purely a
/// starting point for the UI (frequency is acoustically irrelevant at
/// 0 dB gain).
pub const DEFAULT_BAND_FREQS: [f32; 4] = [100.0, 350.0, 3_000.0, 9_000.0];

impl Default for EqParams {
    fn default() -> Self {
        Self::neutral()
    }
}

impl EqParams {
    /// The identity parameter set: master engaged, every band at 0 dB.
    #[must_use]
    pub const fn neutral() -> Self {
        Self {
            enabled: true,
            low: EqBandParams::neutral(DEFAULT_BAND_FREQS[0]),
            low_mid: EqBandParams::neutral(DEFAULT_BAND_FREQS[1]),
            high_mid: EqBandParams::neutral(DEFAULT_BAND_FREQS[2]),
            high: EqBandParams::neutral(DEFAULT_BAND_FREQS[3]),
        }
    }

    /// True when the EQ cannot change the signal: master bypassed, or
    /// every band inert (bypassed or 0 dB). The engine uses this for the
    /// bit-exact bypass decision — exactly the user-facing contract
    /// "bit-exact bypass when the EQ is disabled".
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.enabled
            && [self.low, self.low_mid, self.high_mid, self.high]
                .iter()
                .any(|b| !b.is_inert())
    }

    /// Indexed band access (0 = low shelf … 3 = high shelf).
    #[must_use]
    pub fn band(&self, i: usize) -> EqBandParams {
        match i {
            0 => self.low,
            1 => self.low_mid,
            2 => self.high_mid,
            _ => self.high,
        }
    }

    /// Indexed band mutation (panics for `i ≥ 4` like slice indexing).
    pub fn set_band(&mut self, i: usize, b: EqBandParams) {
        match i {
            0 => self.low = b,
            1 => self.low_mid = b,
            2 => self.high_mid = b,
            _ => self.high = b,
        }
    }

    /// Every band, position order.
    #[must_use]
    pub fn bands(&self) -> [EqBandParams; 4] {
        [self.low, self.low_mid, self.high_mid, self.high]
    }

    /// Clamp every band into range (see [`EqBandParams::sanitized`]) and
    /// keep the master flag.
    #[must_use]
    pub fn sanitized(&self) -> Self {
        Self {
            enabled: self.enabled,
            low: self.low.sanitized(DEFAULT_BAND_FREQS[0]),
            low_mid: self.low_mid.sanitized(DEFAULT_BAND_FREQS[1]),
            high_mid: self.high_mid.sanitized(DEFAULT_BAND_FREQS[2]),
            high: self.high.sanitized(DEFAULT_BAND_FREQS[3]),
        }
    }
}

/// Built-in presets (Phase 8.3 scope). Musical intent documented per
/// preset; frequencies are starting points tuned for voice at 48 kHz.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EqPreset {
    /// All bands at 0 dB (the identity set).
    Flat,
    /// Vocal Presence: clear the low mud, lift the 3.5 kHz presence
    /// ridge and the 9 kHz air shelf.
    VocalPresence,
    /// De-Mud: the classic 250 Hz boxiness cut with a slight top lift
    /// to keep the balance.
    DeMud,
    /// Air Boost: wide, gentle high-shelf lift for breathy brightness.
    AirBoost,
    /// De-Harsh: narrow cut in the 3–5 kHz harshness region with a
    /// small compensating air lift.
    DeHarsh,
}

/// Every preset in display order.
pub const EQ_PRESETS: [EqPreset; 5] = [
    EqPreset::Flat,
    EqPreset::VocalPresence,
    EqPreset::DeMud,
    EqPreset::AirBoost,
    EqPreset::DeHarsh,
];

impl EqPreset {
    /// The preset's parameter set (master engaged, all bands enabled).
    #[must_use]
    pub fn params(self) -> EqParams {
        let mut p = EqParams::neutral();
        let set = |p: &mut EqParams, i: usize, f: f32, q: f32, g: f32| {
            p.set_band(
                i,
                EqBandParams {
                    freq: f,
                    q,
                    gain_db: g,
                    enabled: true,
                },
            );
        };
        match self {
            // Flat: the neutral set verbatim.
            Self::Flat => p,
            Self::VocalPresence => {
                set(&mut p, 0, 100.0, 0.707, -3.0); // remove boom/rumble
                set(&mut p, 1, 300.0, 1.0, -2.0); // de-box the low mids
                set(&mut p, 2, 3_500.0, 0.9, 2.5); // intelligibility ridge
                set(&mut p, 3, 9_000.0, 0.7, 3.0); // air
                p
            }
            Self::DeMud => {
                set(&mut p, 0, 120.0, 0.707, -2.5);
                set(&mut p, 1, 250.0, 1.2, -4.0); // the mud region
                set(&mut p, 2, 3_000.0, 1.0, 1.5); // balance lift
                set(&mut p, 3, 9_000.0, 0.707, 0.0);
                p
            }
            Self::AirBoost => {
                set(&mut p, 0, 100.0, 0.707, 0.0);
                set(&mut p, 1, 350.0, 0.707, 0.0);
                set(&mut p, 2, 6_000.0, 0.8, 1.0); // shimmer onset
                set(&mut p, 3, 10_000.0, 0.6, 6.0); // wide, gentle air
                p
            }
            Self::DeHarsh => {
                set(&mut p, 0, 100.0, 0.707, 0.0);
                set(&mut p, 1, 350.0, 0.707, 0.0);
                set(&mut p, 2, 4_000.0, 2.0, -4.0); // the harshness notch
                set(&mut p, 3, 10_000.0, 0.707, 1.0); // compensate the dulling
                p
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Biquad — RBJ Audio EQ Cookbook coefficients, transposed direct form II
// ---------------------------------------------------------------------------

/// One RBJ biquad section in transposed direct form II (two state
/// accumulators, `f64` internal math).
///
/// Transfer function (a-normalised):
/// `H(z) = (b0 + b1 z⁻¹ + b2 z⁻²) / (1 + a1 z⁻¹ + a2 z⁻²)`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Biquad {
    /// Numerator taps.
    pub b0: f64,
    /// Numerator taps.
    pub b1: f64,
    /// Numerator taps.
    pub b2: f64,
    /// Denominator taps (already normalised by `a0`, sign as in the
    /// difference equation: `y = b0 x + … − a1 y1 − a2 y2`).
    pub a1: f64,
    /// Denominator taps.
    pub a2: f64,
    /// TDF2 state.
    z1: f64,
    /// TDF2 state.
    z2: f64,
}

impl Biquad {
    /// The identity section (used by tests and as a safe default).
    #[must_use]
    pub fn identity() -> Self {
        Self {
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
            a1: 0.0,
            a2: 0.0,
            z1: 0.0,
            z2: 0.0,
        }
    }

    /// Design one section from a band's parameters (RBJ cookbook):
    /// `ω₀ = 2π f₀/Fs`, `α = sin ω₀ / 2Q`, `A = 10^(gain/40)`.
    ///
    /// The requested frequency is clamped under Nyquist (`0.45·Fs`) so
    /// the trigonometry stays finite at every supported rate (a 20 kHz
    /// band on a 44.1 kHz device lands at 19.845 kHz — honest and
    /// stable, and irrelevant at 0 dB gain).
    #[must_use]
    pub fn design(kind: EqBandKind, band: &EqBandParams, rate: u32) -> Self {
        let f0 = f64::from(band.freq.min(0.45 * rate as f32));
        let w0 = 2.0 * std::f64::consts::PI * f0 / f64::from(rate);
        let (sin_w0, cos_w0) = w0.sin_cos();
        let alpha = sin_w0 / (2.0 * f64::from(band.q));
        let a = 10.0f64.powf(f64::from(band.gain_db) / 40.0);
        let two_sqrt_a_alpha = 2.0 * a.sqrt() * alpha;

        let (b0, b1, b2, a0, a1, a2) = match kind {
            EqBandKind::Bell => (
                1.0 + alpha * a,
                -2.0 * cos_w0,
                1.0 - alpha * a,
                1.0 + alpha / a,
                -2.0 * cos_w0,
                1.0 - alpha / a,
            ),
            EqBandKind::LowShelf => (
                a * ((a + 1.0) - (a - 1.0) * cos_w0 + two_sqrt_a_alpha),
                2.0 * a * ((a - 1.0) - (a + 1.0) * cos_w0),
                a * ((a + 1.0) - (a - 1.0) * cos_w0 - two_sqrt_a_alpha),
                (a + 1.0) + (a - 1.0) * cos_w0 + two_sqrt_a_alpha,
                -2.0 * ((a - 1.0) + (a + 1.0) * cos_w0),
                (a + 1.0) + (a - 1.0) * cos_w0 - two_sqrt_a_alpha,
            ),
            EqBandKind::HighShelf => (
                a * ((a + 1.0) + (a - 1.0) * cos_w0 + two_sqrt_a_alpha),
                -2.0 * a * ((a - 1.0) + (a + 1.0) * cos_w0),
                a * ((a + 1.0) + (a - 1.0) * cos_w0 - two_sqrt_a_alpha),
                (a + 1.0) - (a - 1.0) * cos_w0 + two_sqrt_a_alpha,
                2.0 * ((a - 1.0) - (a + 1.0) * cos_w0),
                (a + 1.0) - (a - 1.0) * cos_w0 - two_sqrt_a_alpha,
            ),
        };
        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
            z1: 0.0,
            z2: 0.0,
        }
    }

    /// One sample through TDF2: `y = b0 x + z1`.
    pub fn tick(&mut self, x: f64) -> f64 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }

    /// Clear the state accumulators (stream restart).
    pub fn reset(&mut self) {
        self.z1 = 0.0;
        self.z2 = 0.0;
    }

    /// Magnitude of the transfer function at `freq` (Hz), in dB. Uses
    /// the analytic evaluation of `H(e^{jω})` from the taps.
    #[must_use]
    pub fn magnitude_db(&self, freq: f64, rate: u32) -> f64 {
        let w = 2.0 * std::f64::consts::PI * freq / f64::from(rate);
        let (s, c) = w.sin_cos();
        let (s2, c2) = (2.0 * w).sin_cos();
        let n_re = self.b0 + self.b1 * c + self.b2 * c2;
        let n_im = -(self.b1 * s + self.b2 * s2);
        let d_re = 1.0 + self.a1 * c + self.a2 * c2;
        let d_im = -(self.a1 * s + self.a2 * s2);
        let mag = (n_re * n_re + n_im * n_im).sqrt()
            / ((d_re * d_re + d_im * d_im).sqrt().max(f64::MIN_POSITIVE));
        20.0 * mag.log10()
    }
}

// ---------------------------------------------------------------------------
// Processor — the four sections with per-band redesign bookkeeping
// ---------------------------------------------------------------------------

/// The per-channel EQ processor: four biquads in series, coefficients
/// redesigned lazily when a band's `(freq, q, gain)` triple changes.
///
/// One instance per audio channel (engines are per-channel); state is
/// eight `f64` accumulators total. No allocation after construction.
#[derive(Debug)]
pub struct EqProcessor {
    rate: u32,
    biquads: [Biquad; 4],
    designed: [(f32, f32, f32); 4],
}

impl EqProcessor {
    /// A processor for `rate`, resting at the identity coefficients.
    #[must_use]
    pub fn new(rate: u32) -> Self {
        Self {
            rate,
            biquads: [Biquad::identity(); 4],
            designed: [(f32::NAN, f32::NAN, f32::NAN); 4],
        }
    }

    /// Re-target the sample rate (coefficients redesign on next use).
    pub fn set_rate(&mut self, rate: u32) {
        if self.rate != rate {
            self.rate = rate;
            self.designed = [(f32::NAN, f32::NAN, f32::NAN); 4];
        }
    }

    /// Clear filter state (stream restart).
    pub fn reset(&mut self) {
        for b in &mut self.biquads {
            b.reset();
        }
    }

    /// Process one block in place. A inactive (bypassed/flat) EQ is a
    /// bit-exact no-op; inert bands are skipped individually.
    pub fn process(&mut self, params: &EqParams, io: &mut [f32]) {
        if !params.is_active() {
            return;
        }
        for (i, band) in params.bands().iter().enumerate() {
            if band.is_inert() {
                continue;
            }
            let triple = (band.freq, band.q, band.gain_db);
            if self.designed[i] != triple {
                self.biquads[i] = Biquad::design(band_kind(i), band, self.rate);
                self.designed[i] = triple;
            }
            let bq = &mut self.biquads[i];
            for s in io.iter_mut() {
                *s = bq.tick(f64::from(*s)) as f32;
            }
        }
    }

    /// The EQ's summed magnitude response at `freq` in dB (only active
    /// bands contribute; inert bands contribute exactly 0 dB).
    #[must_use]
    pub fn response_db(&self, params: &EqParams, freq: f64) -> f64 {
        let mut total = 0.0;
        for (i, band) in params.bands().iter().enumerate() {
            if band.is_inert() {
                continue;
            }
            let bq = Biquad::design(band_kind(i), band, self.rate);
            total += bq.magnitude_db(freq, self.rate);
        }
        total
    }
}

/// Log-spaced magnitude response over 20 Hz … 20 kHz for the UI curve:
/// `points` values in dB, linearly spaced in log-frequency
/// (`f(i) = 20 · 1000^(i/(points−1))` — 3 decades, 20 Hz at `i = 0`,
/// 20 kHz at `i = points−1`).
#[must_use]
pub fn response_curve_db(params: &EqParams, rate: u32, points: usize) -> Vec<f64> {
    let n = points.max(2);
    let decades = (EQ_MAX_FREQ_HZ / EQ_MIN_FREQ_HZ) as f64; // 1000 = 3 decades
    (0..n)
        .map(|i| {
            let t = i as f64 / (n - 1) as f64;
            let f = f64::from(EQ_MIN_FREQ_HZ) * decades.powf(t);
            let f = f.min(f64::from(EQ_MAX_FREQ_HZ));
            let mut total = 0.0;
            for (b, band) in params.bands().iter().enumerate() {
                if band.is_inert() {
                    continue;
                }
                total += Biquad::design(band_kind(b), band, rate).magnitude_db(f, rate);
            }
            total
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;

    /// RMS of a slice.
    fn rms(x: &[f32]) -> f64 {
        (x.iter().map(|v| f64::from(*v) * f64::from(*v)).sum::<f64>() / x.len() as f64).sqrt()
    }

    /// Steady-state gain of `bq` at `freq` in dB: feed a sine, discard
    /// the transient, compare RMS. Independent of the analytic
    /// evaluation — this exercises the actual difference equation. The
    /// window spans ≥ 30 periods even at 20 Hz so partial-period RMS
    /// bias stays far below the tolerance.
    fn measured_gain_db(bq: &Biquad, freq: f64, rate: u32) -> f64 {
        let w = 2.0 * std::f64::consts::PI * freq / f64::from(rate);
        let total = 65_536;
        let skip = 8_192;
        let mut bq = *bq;
        let mut in_sq = 0.0;
        let mut out_sq = 0.0;
        for i in 0..total {
            let x = (w * i as f64).sin();
            let y = bq.tick(x);
            if i >= skip {
                in_sq += x * x;
                out_sq += y * y;
            }
        }
        20.0 * (out_sq / in_sq).sqrt().log10()
    }

    /// Independent re-derivation of the cookbook coefficients (literal
    /// transcription, deliberately not sharing code with `Biquad`).
    fn cookbook_reference(
        kind: EqBandKind,
        band: &EqBandParams,
        rate: u32,
    ) -> (f64, f64, f64, f64, f64, f64) {
        let f0 = f64::from(band.freq.min(0.45 * rate as f32));
        let w0 = std::f64::consts::TAU * f0 / f64::from(rate);
        let cw = w0.cos();
        let sw = w0.sin();
        let alpha = sw / (2.0 * f64::from(band.q));
        let aa = 10.0f64.powf(f64::from(band.gain_db) / 40.0);
        let sa = 2.0 * aa.sqrt() * alpha;
        match kind {
            EqBandKind::Bell => (
                1.0 + alpha * aa,
                -2.0 * cw,
                1.0 - alpha * aa,
                1.0 + alpha / aa,
                -2.0 * cw,
                1.0 - alpha / aa,
            ),
            EqBandKind::LowShelf => (
                aa * ((aa + 1.0) - (aa - 1.0) * cw + sa),
                2.0 * aa * ((aa - 1.0) - (aa + 1.0) * cw),
                aa * ((aa + 1.0) - (aa - 1.0) * cw - sa),
                (aa + 1.0) + (aa - 1.0) * cw + sa,
                -2.0 * ((aa - 1.0) + (aa + 1.0) * cw),
                (aa + 1.0) + (aa - 1.0) * cw - sa,
            ),
            EqBandKind::HighShelf => (
                aa * ((aa + 1.0) + (aa - 1.0) * cw + sa),
                -2.0 * aa * ((aa - 1.0) + (aa + 1.0) * cw),
                aa * ((aa + 1.0) + (aa - 1.0) * cw - sa),
                (aa + 1.0) - (aa - 1.0) * cw + sa,
                2.0 * ((aa - 1.0) - (aa + 1.0) * cw),
                (aa + 1.0) - (aa - 1.0) * cw - sa,
            ),
        }
    }

    fn analytic_gain_db(kind: EqBandKind, band: &EqBandParams, freq: f64, rate: u32) -> f64 {
        let (b0, b1, b2, a0, a1, a2) = cookbook_reference(kind, band, rate);
        // |H(e^{jω})| from the *unnormalised* cookbook taps.
        let w = std::f64::consts::TAU * freq / f64::from(rate);
        let (s, c) = w.sin_cos();
        let (s2, c2) = (2.0 * w).sin_cos();
        let n_re = b0 + b1 * c + b2 * c2;
        let n_im = -(b1 * s + b2 * s2);
        let d_re = a0 + a1 * c + a2 * c2;
        let d_im = -(a1 * s + a2 * s2);
        let mag = (n_re * n_re + n_im * n_im).sqrt() / (d_re * d_re + d_im * d_im).sqrt();
        20.0 * mag.log10()
    }

    /// The acceptance test from the phase spec: the *measured* response
    /// (impulse-equation through a real sine) matches the *analytic*
    /// biquad response at 5 test frequencies, for every band kind.
    #[test]
    fn response_matches_analytic_biquad_at_5_frequencies() {
        let cases: [(EqBandKind, EqBandParams); 3] = [
            (
                EqBandKind::LowShelf,
                EqBandParams {
                    freq: 100.0,
                    q: 0.707,
                    gain_db: 6.0,
                    enabled: true,
                },
            ),
            (
                EqBandKind::Bell,
                EqBandParams {
                    freq: 350.0,
                    q: 0.707,
                    gain_db: -8.0,
                    enabled: true,
                },
            ),
            (
                EqBandKind::HighShelf,
                EqBandParams {
                    freq: 9_000.0,
                    q: 0.707,
                    gain_db: 5.5,
                    enabled: true,
                },
            ),
        ];
        for (kind, band) in cases {
            let bq = Biquad::design(kind, &band, RATE);
            let f0 = f64::from(band.freq);
            let fives = [
                f0 / 8.0,
                f0 / 2.0,
                f0,
                f0 * 2.0,
                (f0 * 8.0).min(f64::from(EQ_MAX_FREQ_HZ)),
            ];
            for f in fives {
                let measured = measured_gain_db(&bq, f, RATE);
                let analytic = analytic_gain_db(kind, &band, f, RATE);
                assert!(
                    (measured - analytic).abs() < 0.05,
                    "{kind:?} @ {f:.1} Hz: measured {measured:.4} dB vs analytic {analytic:.4} dB"
                );
            }
        }
    }

    /// The bell's peak gain at `f0` is exactly the requested dB
    /// (amplitude `A² = 10^(dB/20)` — the RBJ `A = 10^(dB/40)`
    /// convention), and shelves hold the boost at their asymptotes
    /// (DC for the low shelf, Nyquist for the high shelf — both exact
    /// analytic identities, cross-checked against a steady-state
    /// measurement inside the transition-free region).
    #[test]
    fn band_gains_hit_their_targets() {
        let bell = EqBandParams {
            freq: 1_000.0,
            q: 0.707,
            gain_db: 12.0,
            enabled: true,
        };
        let bq = Biquad::design(EqBandKind::Bell, &bell, RATE);
        let peak = bq.magnitude_db(1_000.0, RATE);
        assert!((peak - 12.0).abs() < 0.01, "bell peak {peak:.4} dB");

        let low = EqBandParams {
            freq: 100.0,
            q: 0.707,
            gain_db: -9.0,
            enabled: true,
        };
        let bq = Biquad::design(EqBandKind::LowShelf, &low, RATE);
        // Exact asymptote identity: H(1) = A².
        let dc = bq.magnitude_db(0.0, RATE);
        assert!((dc - (-9.0)).abs() < 0.001, "shelf DC {dc:.4} dB");
        // Measured near-DC (transition-free) agrees with the analytic value.
        let m = measured_gain_db(&bq, 25.0, RATE);
        let a = analytic_gain_db(EqBandKind::LowShelf, &low, 25.0, RATE);
        assert!((m - a).abs() < 0.05, "25 Hz: {m:.4} vs analytic {a:.4} dB");

        let high = EqBandParams {
            freq: 8_000.0,
            q: 0.707,
            gain_db: 6.0,
            enabled: true,
        };
        let bq = Biquad::design(EqBandKind::HighShelf, &high, RATE);
        // Exact asymptote identity: H(−1) = A².
        let nyq = bq.magnitude_db(f64::from(RATE) / 2.0, RATE);
        assert!((nyq - 6.0).abs() < 0.001, "air shelf Nyquist {nyq:.4} dB");
        // 15 kHz is still inside the shelf's transition band — the honest
        // check is equation correctness, not the nominal number.
        let m = measured_gain_db(&bq, 15_000.0, RATE);
        let a = analytic_gain_db(EqBandKind::HighShelf, &high, 15_000.0, RATE);
        assert!((m - a).abs() < 0.05, "15 kHz: {m:.4} vs analytic {a:.4} dB");
    }

    /// A resonant bell (Q = 10) still measures true (long-ring case).
    #[test]
    fn high_q_bell_matches_analytic() {
        let band = EqBandParams {
            freq: 1_000.0,
            q: 10.0,
            gain_db: 12.0,
            enabled: true,
        };
        let bq = Biquad::design(EqBandKind::Bell, &band, RATE);
        for f in [500.0, 1_000.0, 2_000.0] {
            let measured = measured_gain_db(&bq, f, RATE);
            let analytic = analytic_gain_db(EqBandKind::Bell, &band, f, RATE);
            assert!(
                (measured - analytic).abs() < 0.05,
                "Q=10 @ {f}: {measured:.4} vs {analytic:.4} dB"
            );
        }
    }

    /// Coefficients survive the full stated parameter range without NaN
    /// or instability (the sinusoid test below also catches overflow).
    #[test]
    fn extreme_ranges_stay_finite_and_stable() {
        for &freq in &[20.0f32, 100.0, 1_000.0, 10_000.0, 20_000.0] {
            for &q in &[0.1f32, 0.5, 1.0, 5.0, 10.0] {
                for &gain in &[-24.0f32, -3.0, 0.0, 3.0, 24.0] {
                    for kind in [
                        EqBandKind::LowShelf,
                        EqBandKind::Bell,
                        EqBandKind::HighShelf,
                    ] {
                        let band = EqBandParams {
                            freq,
                            q,
                            gain_db: gain,
                            enabled: true,
                        };
                        let bq = Biquad::design(kind, &band, RATE);
                        assert!(
                            bq.b0.is_finite()
                                && bq.b1.is_finite()
                                && bq.b2.is_finite()
                                && bq.a1.is_finite()
                                && bq.a2.is_finite(),
                            "{kind:?} f{freq} q{q} g{gain}: non-finite taps"
                        );
                        // BIBO stability: poles inside the unit circle.
                        // Denominator z² + a1 z + a2 (taps already
                        // a-normalised) — Jury: |a2| < 1, |a1| < 1 + a2.
                        assert!(
                            bq.a2.abs() < 1.0 && bq.a1.abs() < 1.0 + bq.a2,
                            "{kind:?} f{freq} q{q} g{gain}: unstable taps"
                        );
                    }
                }
            }
        }
        // Degenerate frequency clamp: 20 kHz on a 44.1 kHz stream must
        // clamp under Nyquist and stay finite.
        let band = EqBandParams {
            freq: 20_000.0,
            q: 0.707,
            gain_db: 6.0,
            enabled: true,
        };
        let bq = Biquad::design(EqBandKind::HighShelf, &band, 44_100);
        assert!(bq.b0.is_finite() && bq.a2.is_finite());
    }

    /// Neutral (flat / disabled) EQ never touches a single bit.
    #[test]
    fn neutral_and_disabled_are_bit_exact() {
        let mut signal: Vec<f32> = (0..4_096)
            .map(|i| ((i as f32 * 0.037).sin() * 0.42).min(0.9))
            .collect();
        signal[100] = -0.0; // sign-of-zero sentinel

        let neutral = EqParams::neutral();
        assert!(!neutral.is_active());
        let mut p = EqProcessor::new(RATE);
        let mut out = signal.clone();
        p.process(&neutral, &mut out);
        assert_eq!(out, signal, "flat EQ must be bit-exact");

        let disabled = EqParams {
            enabled: false,
            low: EqBandParams {
                freq: 100.0,
                q: 1.0,
                gain_db: 12.0,
                enabled: true,
            },
            ..EqParams::neutral()
        };
        assert!(!disabled.is_active(), "master bypass wins over band gains");
        let mut out = signal.clone();
        p.process(&disabled, &mut out);
        assert_eq!(out, signal, "disabled EQ must be bit-exact");

        // Enabled master but every band bypassed → still inert.
        let bands_off = EqParams {
            low: EqBandParams {
                freq: 100.0,
                q: 1.0,
                gain_db: 6.0,
                enabled: false,
            },
            high: EqBandParams {
                freq: 9_000.0,
                q: 1.0,
                gain_db: 6.0,
                enabled: false,
            },
            ..EqParams::neutral()
        };
        assert!(!bands_off.is_active());
        let mut out = signal.clone();
        p.process(&bands_off, &mut out);
        assert_eq!(out, signal, "all-bands-bypassed must be bit-exact");
    }

    /// An active EQ changes the signal, and skipping inert bands equals
    /// processing only the active ones.
    #[test]
    fn active_bands_process_and_inert_bands_skip() {
        let signal: Vec<f32> = (0..8_192)
            .map(|i| {
                (2.0 * std::f64::consts::PI * 1_000.0 * i as f64 / f64::from(RATE)).sin() as f32
                    * 0.5
            })
            .collect();

        let mut one = EqParams::neutral();
        one.set_band(
            2,
            EqBandParams {
                freq: 3_000.0,
                q: 1.0,
                gain_db: 12.0,
                enabled: true,
            },
        );
        assert!(one.is_active());

        let mut p = EqProcessor::new(RATE);
        let mut out = signal.clone();
        p.process(&one, &mut out);
        assert_ne!(out, signal, "active band must alter the signal");
        assert!(
            (rms(&out) - rms(&signal)).abs() > 1e-4,
            "a 12 dB boost at 3 kHz must move a broadband signal's RMS"
        );

        // The other three bands inert: a FRESH processor with explicit
        // inert triples must give the identical output (skipping ==
        // identity coefficients; the shared processor would carry state
        // from the first run and defeat the comparison).
        let mut two = one;
        two.low = EqBandParams {
            freq: 100.0,
            q: 0.707,
            gain_db: 0.0,
            enabled: true,
        };
        two.low_mid = EqBandParams {
            freq: 350.0,
            q: 0.707,
            gain_db: 4.0,
            enabled: false,
        };
        two.high = EqBandParams {
            freq: 9_000.0,
            q: 0.707,
            gain_db: 0.0,
            enabled: false,
        };
        let mut p2 = EqProcessor::new(RATE);
        let mut out2 = signal.clone();
        p2.process(&two, &mut out2);
        assert_eq!(out, out2, "inert bands must be exact no-ops");
    }

    /// The processor's summed response equals the product of the active
    /// bands' analytic responses.
    #[test]
    fn processor_response_sums_bands_in_db() {
        let mut params = EqPreset::VocalPresence.params();
        params = params.sanitized();
        let p = EqProcessor::new(RATE);
        for f in [50.0, 300.0, 1_000.0, 3_500.0, 12_000.0] {
            let total = p.response_db(&params, f);
            let manual: f64 = params
                .bands()
                .iter()
                .enumerate()
                .filter(|(_, b)| !b.is_inert())
                .map(|(i, b)| analytic_gain_db(band_kind(i), b, f, RATE))
                .sum();
            assert!(
                (total - manual).abs() < 0.01,
                "response sum at {f}: {total:.4} vs {manual:.4} dB"
            );
        }
    }

    /// The curve helper: log grid, flat when neutral, sane when active.
    #[test]
    fn response_curve_grid_and_values() {
        let n = EQ_CURVE_POINTS;
        let flat = response_curve_db(&EqParams::neutral(), RATE, n);
        assert_eq!(flat.len(), n);
        assert!(
            flat.iter().all(|v| v.abs() < 1e-9),
            "flat curve must be 0 dB"
        );

        let mut active = EqParams::neutral();
        active.set_band(
            3,
            EqBandParams {
                freq: 9_000.0,
                q: 0.707,
                gain_db: 6.0,
                enabled: true,
            },
        );
        let curve = response_curve_db(&active, RATE, n);
        // Endpoints: 20 Hz (far below the shelf corner) ≈ 0 dB; 20 kHz
        // (above it) carries most of the shelf gain.
        assert!(curve[0].abs() < 0.1, "20 Hz of an air shelf must be ~0 dB");
        assert!(
            curve[n - 1] > 5.0,
            "20 kHz must be deep into the shelf boost: {}",
            curve[n - 1]
        );

        // Log grid: the centre point is 20·10^1.5 ≈ 632 Hz — far below a
        // 9 kHz shelf, so ≈ 0 dB there.
        let centre = response_curve_db(&active, RATE, n)[n / 2];
        assert!(
            centre.abs() < 0.5,
            "632 Hz far from a 9 kHz shelf: {centre}"
        );
    }

    /// Parameter sanitization: clamps, non-finite fallbacks, master flag.
    #[test]
    fn sanitization_clamps_and_falls_back() {
        let wild = EqParams {
            enabled: false,
            low: EqBandParams {
                freq: 5.0,
                q: 100.0,
                gain_db: 99.0,
                enabled: true,
            },
            low_mid: EqBandParams {
                freq: f32::NAN,
                q: f32::INFINITY,
                gain_db: f32::NEG_INFINITY,
                enabled: true,
            },
            high_mid: EqBandParams {
                freq: 30_000.0,
                q: 0.01,
                gain_db: -99.0,
                enabled: true,
            },
            high: EqBandParams {
                freq: 9_000.0,
                q: 0.707,
                gain_db: 0.0,
                enabled: true,
            },
        };
        let s = wild.sanitized();
        assert!(!s.enabled, "master flag survives sanitization");
        assert_eq!(s.low.freq, EQ_MIN_FREQ_HZ);
        assert_eq!(s.low.q, EQ_MAX_Q);
        assert_eq!(s.low.gain_db, EQ_MAX_GAIN_DB);
        assert_eq!(s.low_mid.freq, DEFAULT_BAND_FREQS[1], "NaN → default");
        assert_eq!(s.low_mid.q, 0.707, "Inf → default Q");
        assert_eq!(s.low_mid.gain_db, 0.0, "−Inf → default gain");
        assert_eq!(s.high_mid.freq, EQ_MAX_FREQ_HZ);
        assert_eq!(s.high_mid.q, EQ_MIN_Q);
        assert_eq!(s.high_mid.gain_db, -EQ_MAX_GAIN_DB);
        assert!(!s.is_active(), "nothing above is audible: flat + bypassed");
    }

    /// Presets: shape sanity (Flat is the identity; the others are
    /// active with the documented band moves).
    #[test]
    fn presets_have_documented_shapes() {
        assert!(!EqPreset::Flat.params().is_active());
        for p in [
            EqPreset::VocalPresence,
            EqPreset::DeMud,
            EqPreset::AirBoost,
            EqPreset::DeHarsh,
        ] {
            let params = p.params();
            assert!(params.is_active(), "{p:?} must be active");
            assert!(params.bands().iter().all(|b| b.enabled));
            let s = params.sanitized();
            assert_eq!(s, params, "{p:?} must already be inside the ranges");
        }
        let vp = EqPreset::VocalPresence.params();
        assert!((vp.low.gain_db - (-3.0)).abs() < 1e-6);
        assert!((vp.high_mid.gain_db - 2.5).abs() < 1e-6);
        let dh = EqPreset::DeHarsh.params();
        assert!((dh.high_mid.gain_db - (-4.0)).abs() < 1e-6);
        assert!((dh.high_mid.freq - 4_000.0).abs() < 1e-6);
    }

    /// Rate changes redesign the coefficients: the same band at two
    /// rates is a different filter (different taps), both stable, both
    /// hitting the requested peak, and the processor actually re-designs
    /// after `set_rate` (behavioural: its output for a Nyquist-adjacent
    /// tone changes).
    #[test]
    fn rate_change_redesigns() {
        let mut params = EqParams::neutral();
        params.set_band(
            2,
            EqBandParams {
                freq: 3_000.0,
                q: 2.0,
                gain_db: 12.0,
                enabled: true,
            },
        );
        let b48 = Biquad::design(EqBandKind::Bell, &params.band(2), 48_000);
        let b96 = Biquad::design(EqBandKind::Bell, &params.band(2), 96_000);
        assert_ne!(b48, b96, "taps must differ across rates");
        // The bell's peak at f0 is A² regardless of rate.
        assert!((b48.magnitude_db(3_000.0, 48_000) - 12.0).abs() < 0.01);
        assert!((b96.magnitude_db(3_000.0, 96_000) - 12.0).abs() < 0.01);

        // Behavioural: a 22 kHz tone sits above the 3 kHz bell's centre;
        // how far above (in octaves AND in normalised ω) depends on the
        // rate, so the processed output must change after set_rate.
        let mut p = EqProcessor::new(48_000);
        let signal: Vec<f32> = (0..4_096)
            .map(|i| {
                (2.0 * std::f64::consts::PI * 22_000.0 * i as f64 / 96_000.0).sin() as f32 * 0.5
            })
            .collect();
        let mut a = signal.clone();
        p.process(&params, &mut a);
        p.set_rate(96_000);
        let mut b = signal.clone();
        p.process(&params, &mut b);
        assert_ne!(a, b, "set_rate must redesign, not keep the old taps");
    }
}
