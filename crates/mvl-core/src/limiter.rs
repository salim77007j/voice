//! True-peak safety guard (plan §6.5, stage 4).
//!
//! A transparency guard, not a compressor: it engages **only** when the
//! processed output would exceed `min(source true peak + 0.3 dB,
//! −0.1 dBTP)` — which the test suite asserts never happens for
//! moderate settings. Two modes:
//!
//! * offline (`guard_offline`): one whole-buffer measurement with a
//!   4×-oversampled true-peak estimate (windowed-sinc via rubato,
//!   streaming-folded so memory stays O(chunk)), then a single clean
//!   gain change;
//! * streaming (used by the engine per emitted block): sample-peak
//!   against the absolute −0.1 dBTP ceiling only — the source-relative
//!   bound needs the whole source, which a live stream does not have
//!   yet (documented honestly).

use crate::pitch::RatioConverter;

/// Absolute ceiling: −0.1 dBTP.
pub const CEILING_DBTP: f64 = -0.1;
/// Allowed overshoot above the source true peak.
pub const SOURCE_HEADROOM_DB: f64 = 0.3;

/// Maximum absolute sample value.
#[must_use]
pub fn sample_peak(x: &[f32]) -> f64 {
    x.iter().fold(0.0f64, |m, v| m.max(f64::from(v.abs())))
}

/// 4×-oversampled true-peak estimate (streaming fold, O(chunk) memory).
#[must_use]
pub fn true_peak(x: &[f32]) -> f64 {
    if x.is_empty() {
        return 0.0;
    }
    let mut peak = sample_peak(x);
    let Ok(mut rc) = RatioConverter::new(4.0) else {
        return peak;
    };
    let mut fold = |rc: &mut RatioConverter| loop {
        let out = rc.pop_available(65_536);
        if out.is_empty() {
            break;
        }
        for v in out {
            peak = peak.max(v.abs());
        }
    };
    for chunk in x.chunks(8192) {
        let inp: Vec<f64> = chunk.iter().map(|v| f64::from(*v)).collect();
        rc.push(&inp);
        fold(&mut rc);
    }
    rc.flush();
    fold(&mut rc);
    peak
}

/// Offline guard: scale `out` down so its true peak honours
/// `min(src_true_peak + 0.3 dB, −0.1 dBTP)`. Returns whether it engaged.
pub fn guard_offline(out: &mut [f32], src_true_peak: f64) -> bool {
    let ceiling = (src_true_peak * 10.0f64.powf(SOURCE_HEADROOM_DB / 20.0))
        .min(10.0f64.powf(CEILING_DBTP / 20.0));
    let out_tp = true_peak(out);
    if out_tp > ceiling && out_tp > 0.0 {
        let g = (ceiling / out_tp) as f32;
        for v in out.iter_mut() {
            *v *= g;
        }
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn true_peak_catches_inter_sample_peaks() {
        // Sine at fs/8 with pi/8 phase: samples hit 0.383·A and 0.924·A
        // while the continuous peak is A — the classic inter-sample
        // overshoot case.
        let rate = 4800.0f64;
        let amp = 0.8;
        let x: Vec<f32> = (0..4800)
            .map(|i| {
                let t = i as f64 / rate;
                (amp * (2.0 * std::f64::consts::PI * (rate / 8.0) * t
                    + std::f64::consts::FRAC_PI_8)
                    .sin()) as f32
            })
            .collect();
        let sample_peak = sample_peak(&x);
        let tp = true_peak(&x);
        assert!(
            sample_peak < amp * 0.95,
            "fixture broken: sample peak {sample_peak:.3}"
        );
        assert!(
            tp > amp * 0.97,
            "true peak {tp:.3} missed the inter-sample peak (~{amp})"
        );
        assert!(tp <= amp * 1.06, "true peak overshoot {tp:.3}");
    }

    #[test]
    fn guard_engages_only_above_ceiling() {
        let src = vec![0.5f32; 4800];
        let mut quiet = vec![0.4f32; 4800];
        assert!(!guard_offline(&mut quiet, true_peak(&src)));
        assert!(quiet.iter().all(|v| *v == 0.4));
        let mut hot = vec![0.99f32; 4800];
        assert!(guard_offline(&mut hot, true_peak(&src)));
        let ceiling = 10.0f64.powf(CEILING_DBTP / 20.0);
        assert!(
            sample_peak(&hot) <= ceiling + 1e-4,
            "guarded peak {:.4} exceeds ceiling",
            sample_peak(&hot)
        );
    }

    #[test]
    fn guard_respects_source_headroom() {
        // Quiet source (peak 0.2): output at 0.35 exceeds src + 0.3 dB
        // (0.2 * 1.035 = 0.207) and must be pulled back below that.
        let src = vec![0.2f32; 4800];
        let mut out = vec![0.35f32; 4800];
        assert!(guard_offline(&mut out, true_peak(&src)));
        assert!(sample_peak(&out) <= 0.208, "peak {:.4}", sample_peak(&out));
    }

    #[test]
    fn empty_input_is_safe() {
        assert_eq!(true_peak(&[]), 0.0);
        let mut empty: Vec<f32> = Vec::new();
        assert!(!guard_offline(&mut empty, 0.0));
    }
}
