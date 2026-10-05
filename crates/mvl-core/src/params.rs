//! Vocal processing parameters shared by every stage of Micro-Vocal Lab.
//!
//! The three precision modules of the product map 1:1 onto the three
//! fields of [`VocalParams`]. All values are plain `Copy` data so the
//! struct can be published to the real-time audio path through a lock-free
//! slot without touching the allocator or a mutex
//! (`docs/ARCHITECTURE_PLAN.md` §6.5).

/// Perceived vocal-tract length that maps to a neutral (identity) formant
/// setting. 170 mm is the average adult-male vocal tract length and the
/// reference point of the formant slider (§6.3 of the architecture plan).
pub const NEUTRAL_TRACT_MM: f32 = 170.0;

/// Minimum selectable vocal-tract length (child-sized perception).
pub const MIN_TRACT_MM: f32 = 100.0;

/// Maximum selectable vocal-tract length (very large adult perception).
pub const MAX_TRACT_MM: f32 = 260.0;

use crate::eq::EqParams;

/// Pitch shift range in semitones (±1 octave).
pub const MAX_PITCH_SEMITONES: f32 = 12.0;

/// Air & breath slider range in percent of full effect (both directions).
pub const MAX_AIR_PERCENT: i32 = 100;

/// The three precision sliders plus the channel EQ, as published to the
/// DSP engine.
///
/// Invariants are enforced by [`VocalParams::sanitized`]: every field is
/// clamped into its documented range so the engine can rely on the values
/// without defensive checks in inner loops.
///
/// Phase 8.3 note: the struct grew by the EQ block (4 bands × 4 fields +
/// master flag ≈ 68 bytes, still plain `Copy`). It travels the same
/// parameter channel as before (`PreviewCmd::SetParams`, `Mutex` slot) —
/// never the audio callback — so the lock-free/real-time contract is
/// unchanged; the versioned `#[repr(C)]` block from the strategy risk
/// table only becomes necessary if params ever move through raw atomics.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct VocalParams {
    /// Pitch shift in semitones, range `−12.00 ..= +12.00`.
    /// Resolution: 0.01 st (1 cent).
    pub pitch_semitones: f32,
    /// Air & breath control, range `−100 ..= +100` percent.
    /// Negative removes breath/sibilance, positive adds warm airiness.
    pub air_percent: i32,
    /// Perceived vocal-tract length in millimetres, range
    /// `100.0 ..= 260.0`. [`NEUTRAL_TRACT_MM`] is the identity.
    pub tract_mm: f32,
    /// Four-band parametric EQ (Phase 8.3), applied after the vocal
    /// chain's true-peak guard. Neutral/flat/disabled = bit-exact.
    pub eq: EqParams,
}

impl VocalParams {
    /// The identity parameter set: processing with these values must be a
    /// bit-exact passthrough (engine invariant #1, plan §6.7).
    pub const fn neutral() -> Self {
        Self {
            pitch_semitones: 0.0,
            air_percent: 0,
            tract_mm: NEUTRAL_TRACT_MM,
            eq: EqParams::neutral(),
        }
    }

    /// True when the *vocal* sliders are at the identity (pitch, air,
    /// tract) — regardless of the EQ, which has its own bypass logic.
    #[must_use]
    pub fn vocal_neutral(&self) -> bool {
        self.pitch_semitones == 0.0 && self.air_percent == 0 && self.tract_mm == NEUTRAL_TRACT_MM
    }

    /// True when this set equals [`VocalParams::neutral`] — vocal sliders
    /// neutral **and** the EQ acoustically inert (master bypassed, or
    /// every band bypassed/flat). The engine uses this to engage the
    /// bit-exact bypass path.
    pub fn is_neutral(&self) -> bool {
        self.vocal_neutral() && !self.eq.is_active()
    }

    /// Return a copy with every field clamped into its documented range.
    /// Also canonicalizes a zero pitch value to exactly `0.0` and a
    /// non-finite pitch (which cannot occur from the UI slider, but might
    /// from hand-written code) to the neutral value — the engine must
    /// never see NaN.
    pub fn sanitized(&self) -> Self {
        let pitch = if self.pitch_semitones.is_finite() {
            self.pitch_semitones
                .clamp(-MAX_PITCH_SEMITONES, MAX_PITCH_SEMITONES)
        } else {
            0.0
        };
        let pitch = if pitch == 0.0 { 0.0 } else { pitch };
        Self {
            pitch_semitones: pitch,
            air_percent: self.air_percent.clamp(-MAX_AIR_PERCENT, MAX_AIR_PERCENT),
            tract_mm: if self.tract_mm.is_finite() {
                self.tract_mm.clamp(MIN_TRACT_MM, MAX_TRACT_MM)
            } else {
                NEUTRAL_TRACT_MM
            },
            eq: self.eq.sanitized(),
        }
    }

    /// Pitch shift expressed as a frequency ratio `2^(semitones/12)`.
    /// For the neutral value this is exactly `1.0`.
    pub fn pitch_ratio(&self) -> f32 {
        if self.pitch_semitones == 0.0 {
            1.0
        } else {
            (self.pitch_semitones / 12.0).exp2()
        }
    }

    /// Formant warp ratio `g = L_ref / L` (uniform-tube physics, plan §6.3).
    /// `g > 1` shrinks the perceived tract (frequencies move up),
    /// `g < 1` enlarges it (frequencies move down).
    pub fn formant_ratio(&self) -> f32 {
        NEUTRAL_TRACT_MM / self.tract_mm
    }
}

/// Engine quality profile: preview (low latency) vs render (maximum
/// quality). Both profiles run the *same* algorithm code with different
/// block parameters (plan §6.6) — what you hear is what you export.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QualityProfile {
    /// Real-time preview: 512-point STFT, 128-sample hop at 48 kHz,
    /// engineered for < 20 ms total latency.
    Preview,
    /// Offline render/export: 2048-point STFT, 512-sample hop, full-rate
    /// input, frame-parallel.
    Render,
}

impl QualityProfile {
    /// STFT analysis size in samples at the profile's reference rate.
    pub fn fft_size(&self) -> usize {
        match self {
            Self::Preview => 512,
            Self::Render => 2048,
        }
    }

    /// STFT hop in samples (75% overlap, Hann window).
    pub fn hop_size(&self) -> usize {
        match self {
            Self::Preview => 128,
            Self::Render => 512,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn neutral_is_identity() {
        let p = VocalParams::neutral();
        assert_eq!(p.pitch_semitones, 0.0);
        assert_eq!(p.air_percent, 0);
        assert_eq!(p.tract_mm, NEUTRAL_TRACT_MM);
        assert!(!p.eq.is_active());
        assert!(p.is_neutral());
        assert!(p.vocal_neutral());
        assert_eq!(p.pitch_ratio(), 1.0);
        assert_eq!(p.formant_ratio(), 1.0);
    }

    /// Phase 8.3: EQ-only engagement must not spoil the vocal-chain
    /// neutrality decisions, and a bypassed/flat EQ keeps neutrality.
    #[test]
    fn eq_fields_participate_in_neutrality() {
        let mut p = VocalParams::neutral();
        p.eq.low.gain_db = 3.0;
        assert!(!p.is_neutral(), "active EQ = not neutral");
        assert!(p.vocal_neutral(), "but the vocal sliders still are");

        p.eq.enabled = false;
        assert!(p.is_neutral(), "master-bypassed EQ is inert again");

        p.eq.enabled = true;
        p.eq.low.enabled = false;
        assert!(p.is_neutral(), "bypassed band with gain is inert");
    }

    #[test]
    fn sanitization_clamps_all_fields() {
        let p = VocalParams {
            pitch_semitones: 99.0,
            air_percent: 500,
            tract_mm: 5.0,
            ..VocalParams::neutral()
        }
        .sanitized();
        assert_eq!(p.pitch_semitones, MAX_PITCH_SEMITONES);
        assert_eq!(p.air_percent, MAX_AIR_PERCENT);
        assert_eq!(p.tract_mm, MIN_TRACT_MM);

        let p = VocalParams {
            pitch_semitones: -99.0,
            air_percent: -500,
            tract_mm: 900.0,
            ..VocalParams::neutral()
        }
        .sanitized();
        assert_eq!(p.pitch_semitones, -MAX_PITCH_SEMITONES);
        assert_eq!(p.air_percent, -MAX_AIR_PERCENT);
        assert_eq!(p.tract_mm, MAX_TRACT_MM);
    }

    /// Phase 8.3: the EQ block is sanitized together with the sliders.
    #[test]
    fn sanitization_covers_the_eq() {
        let mut p = VocalParams::neutral();
        p.eq.high_mid.freq = 99_999.0;
        p.eq.high_mid.gain_db = 99.0;
        let s = p.sanitized();
        assert_eq!(s.eq.high_mid.freq, crate::eq::EQ_MAX_FREQ_HZ);
        assert_eq!(s.eq.high_mid.gain_db, crate::eq::EQ_MAX_GAIN_DB);
        // Sanitizing twice is a fixpoint (idempotent).
        assert_eq!(s.sanitized(), s);
    }

    #[test]
    fn sanitization_rejects_non_finite() {
        let p = VocalParams {
            pitch_semitones: f32::NAN,
            air_percent: 0,
            tract_mm: f32::INFINITY,
            ..VocalParams::neutral()
        }
        .sanitized();
        assert_eq!(p.pitch_semitones, 0.0);
        assert_eq!(p.tract_mm, NEUTRAL_TRACT_MM);
        assert!(p.is_neutral());
    }

    #[test]
    fn pitch_ratio_matches_equal_temperament() {
        let p = VocalParams {
            pitch_semitones: 12.0,
            ..VocalParams::neutral()
        };
        assert!((p.pitch_ratio() - 2.0).abs() < 1e-6);
        let p = VocalParams {
            pitch_semitones: -12.0,
            ..VocalParams::neutral()
        };
        assert!((p.pitch_ratio() - 0.5).abs() < 1e-6);
        let p = VocalParams {
            pitch_semitones: 7.0,
            ..VocalParams::neutral()
        };
        assert!((p.pitch_ratio() - 1.498_307_1).abs() < 1e-5);
    }

    #[test]
    fn formant_ratio_direction() {
        // Shorter tract → formants move up → ratio > 1.
        let child = VocalParams {
            tract_mm: 110.0,
            ..VocalParams::neutral()
        };
        assert!(child.formant_ratio() > 1.0);
        // Longer tract → formants move down → ratio < 1.
        let large = VocalParams {
            tract_mm: 230.0,
            ..VocalParams::neutral()
        };
        assert!(large.formant_ratio() < 1.0);
    }

    #[test]
    fn profile_shapes() {
        assert_eq!(QualityProfile::Preview.fft_size(), 512);
        assert_eq!(QualityProfile::Preview.hop_size(), 128);
        assert_eq!(QualityProfile::Render.fft_size(), 2048);
        assert_eq!(QualityProfile::Render.hop_size(), 512);
    }
}
