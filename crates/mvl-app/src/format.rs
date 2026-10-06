//! Locale-neutral formatting and parsing helpers.
//!
//! All user-visible numbers flow through here so the display is
//! consistent and testable: timecodes (`HH:MM:SS.mmm`) in IBM Plex Mono,
//! musical note names for the pitch module, and lenient numeric-field
//! parsing that accepts what a human would type (`+3`, `3.`, `-.5`, `170`).

/// Format a position/duration as `HH:MM:SS.mmm` (plan §8.2).
#[must_use]
pub fn timecode(seconds: f64) -> String {
    let s = if seconds.is_finite() {
        seconds.max(0.0)
    } else {
        0.0
    };
    let total_ms = (s * 1000.0).round() as u64;
    let ms = total_ms % 1000;
    let total_s = total_ms / 1000;
    let sec = total_s % 60;
    let total_min = total_s / 60;
    let min = total_min % 60;
    let h = total_min / 60;
    format!("{h:02}:{min:02}:{sec:02}.{ms:03}")
}

/// Compact elapsed for the recording status (`MM:SS`).
#[must_use]
pub fn elapsed_mmss(seconds: f64) -> String {
    let s = if seconds.is_finite() {
        seconds.max(0.0)
    } else {
        0.0
    };
    let total = s.round() as u64;
    format!("{:02}:{:02}", total / 60, total % 60)
}

/// Note name + cents for a pitch shift expressed from A4 (440 Hz).
///
/// `+3.00` semitones renders as `C 5 +0¢` (A4 + 3 st = C5 — MIDI
/// octave numbering, where the octave increments at C); the cents part
/// is always shown (plan §8.3).
#[must_use]
pub fn note_from_semitones(semitones: f32) -> String {
    const NAMES: [&str; 12] = [
        "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
    ];
    if !semitones.is_finite() {
        return "—".into();
    }
    let total_cents = (semitones * 100.0).round() as i32;
    let st = total_cents.div_euclid(100);
    let cents = total_cents.rem_euclid(100);
    // A4 = MIDI 69; octave numbers increment at C (MIDI 60 = C4)
    let midi = 69 + st;
    let name = NAMES[midi.rem_euclid(12) as usize];
    let octave = midi.div_euclid(12) - 1;
    format!("{name} {octave} +{cents}¢")
}

/// Interval (in semitones) between the reference tract length (170 mm) and
/// `tract_mm`, shown next to the formant value (plan §8.3 "mm + interval").
#[must_use]
pub fn tract_interval_st(tract_mm: f32) -> String {
    let st = 12.0 * (mvl_core::params::NEUTRAL_TRACT_MM / tract_mm).log2();
    if st.abs() < 0.005 {
        "interval +0.00 st".into()
    } else {
        format!("interval {st:+.2} st")
    }
}

/// Format the air & breath readout: percent → dB display scale.
///
/// The scale mirrors the engine's: −100 % = −40 dB full breath
/// reduction (plan §6.4 / §8.3's “−12.0 dB” example = −30 %);
/// +100 % = +40 dB of air injection on the symmetric scale. The live
/// *applied* meter comes from engine telemetry instead.
#[must_use]
pub fn air_db_text(air_percent: i32) -> String {
    let db = air_percent as f32 * 0.4;
    format!("{db:+.1}")
}

/// The percent value implied by a typed dB field entry.
#[must_use]
pub fn air_percent_from_db_text(text: &str) -> Option<i32> {
    let db = parse_f32(text)?;
    let percent = db / 0.4;
    if (-100.0..=100.0).contains(&percent) {
        Some(percent.round() as i32)
    } else {
        None
    }
}

/// Lenient float parsing for the numeric fields: accepts `+3`, `3.`, `-.5`,
/// surrounding whitespace; rejects anything else (empty, two dots, text).
#[must_use]
pub fn parse_f32(text: &str) -> Option<f32> {
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    // reject anything not in [-+0-9.]
    if !t
        .chars()
        .all(|c| c.is_ascii_digit() || c == '.' || c == '-' || c == '+')
    {
        return None;
    }
    let dots = t.matches('.').count();
    if dots > 1 {
        return None;
    }
    t.parse::<f32>().ok().filter(|v| v.is_finite())
}

/// Canonical text for the pitch field: `+3.00` (2 decimals = cent display).
#[must_use]
pub fn pitch_field_text(semitones: f32) -> String {
    format!("{semitones:+.2}")
}

/// Canonical text for the air field: signed dB with 1 decimal.
#[must_use]
pub fn air_field_text(air_percent: i32) -> String {
    air_db_text(air_percent)
}

/// Canonical text for the formant field: plain millimetres.
#[must_use]
pub fn tract_field_text(tract_mm: f32) -> String {
    format!("{tract_mm:.0}")
}

/// Canonical text for an EQ frequency readout: Hz under 1 kHz, kHz above
/// (Western digits and mono units per the localization conventions, §8.6).
#[must_use]
pub fn eq_freq_text(freq: f32) -> String {
    if freq.abs() >= 1000.0 {
        format!("{:.2} kHz", freq / 1000.0)
    } else {
        format!("{freq:.0} Hz")
    }
}

/// Canonical text for an EQ Q readout.
#[must_use]
pub fn eq_q_text(q: f32) -> String {
    format!("{q:.2}")
}

/// Canonical text for an EQ gain readout: signed dB, 1 decimal.
#[must_use]
pub fn eq_gain_text(gain_db: f32) -> String {
    format!("{gain_db:+.1} dB")
}

// ---- compressor (Phase 8.4) ------------------------------------------------

/// Canonical text for the compressor threshold readout: a *level*, so no
/// sign padding (`-20.0 dB`, `0.0 dB`).
#[must_use]
pub fn comp_threshold_text(threshold_db: f32) -> String {
    format!("{threshold_db:.1} dB")
}

/// Canonical text for a ratio readout: `3.5:1`.
#[must_use]
pub fn comp_ratio_text(ratio: f32) -> String {
    format!("{ratio:.1}:1")
}

/// Canonical text for a time-constant readout: one decimal always
/// (`10.0 ms`, `250.0 ms`) so the column width never jumps.
#[must_use]
pub fn comp_ms_text(ms: f32) -> String {
    format!("{ms:.1} ms")
}

/// Canonical text for the knee width readout.
#[must_use]
pub fn comp_knee_text(knee_db: f32) -> String {
    format!("{knee_db:.1} dB")
}

/// Canonical text for the makeup readout: signed dB (wet path gain).
#[must_use]
pub fn comp_makeup_text(makeup_db: f32) -> String {
    format!("{makeup_db:+.1} dB")
}

/// Canonical text for the mix readout: whole percent.
#[must_use]
pub fn comp_mix_text(percent: f32) -> String {
    format!("{percent:.0} %")
}

/// Canonical text for the live gain-reduction meter: signed dB, 1
/// decimal, always ≤ 0 (`-3.2 dB`); `0.0 dB` when idle.
#[must_use]
pub fn comp_gr_text(gr_db: f32) -> String {
    let gr = if gr_db.is_finite() {
        gr_db.min(0.0)
    } else {
        0.0
    };
    format!("{gr:.1} dB")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timecode_formats() {
        assert_eq!(timecode(0.0), "00:00:00.000");
        assert_eq!(timecode(4.182), "00:00:04.182");
        assert_eq!(timecode(71.94), "00:01:11.940");
        assert_eq!(timecode(3661.5), "01:01:01.500");
        assert_eq!(timecode(83.9994), "00:01:23.999");
    }

    #[test]
    fn timecode_clamps_bad_input() {
        assert_eq!(timecode(-5.0), "00:00:00.000");
        assert_eq!(timecode(f64::NAN), "00:00:00.000");
    }

    #[test]
    fn elapsed_mmss_formats() {
        assert_eq!(elapsed_mmss(0.0), "00:00");
        assert_eq!(elapsed_mmss(72.4), "01:12");
        assert_eq!(elapsed_mmss(600.0), "10:00");
    }

    #[test]
    fn note_names_wrap_octaves() {
        assert_eq!(note_from_semitones(0.0), "A 4 +0¢");
        assert_eq!(note_from_semitones(3.0), "C 5 +0¢");
        assert_eq!(note_from_semitones(3.07), "C 5 +7¢");
        assert_eq!(note_from_semitones(-12.0), "A 3 +0¢");
        assert_eq!(note_from_semitones(12.0), "A 5 +0¢");
        assert_eq!(note_from_semitones(1.0), "A# 4 +0¢");
        assert_eq!(note_from_semitones(11.99), "G# 5 +99¢");
        assert_eq!(note_from_semitones(f32::NAN), "—");
    }

    #[test]
    fn tract_interval_signs() {
        assert_eq!(tract_interval_st(170.0), "interval +0.00 st");
        assert!(tract_interval_st(100.0).starts_with("interval +"));
        assert!(tract_interval_st(260.0).starts_with("interval -"));
    }

    #[test]
    fn air_db_scale() {
        assert_eq!(air_db_text(0), "+0.0");
        assert_eq!(air_db_text(-100), "-40.0");
        assert_eq!(air_db_text(100), "+40.0");
        // plan §8.3 example: −12.0 dB readout
        assert_eq!(air_db_text(-30), "-12.0");
    }

    #[test]
    fn air_percent_roundtrip() {
        assert_eq!(air_percent_from_db_text("-40"), Some(-100));
        assert_eq!(air_percent_from_db_text("+40"), Some(100));
        assert_eq!(air_percent_from_db_text("0"), Some(0));
        assert_eq!(air_percent_from_db_text("-12.0"), Some(-30));
        assert_eq!(air_percent_from_db_text("999"), None);
        assert_eq!(air_percent_from_db_text("abc"), None);
    }

    #[test]
    fn parse_lenient_floats() {
        assert_eq!(parse_f32("+3"), Some(3.0));
        assert_eq!(parse_f32(" 3.5 "), Some(3.5));
        assert_eq!(parse_f32("-.5"), Some(-0.5));
        assert_eq!(parse_f32("3."), Some(3.0));
        assert_eq!(parse_f32(""), None);
        assert_eq!(parse_f32("3.1.4"), None);
        assert_eq!(parse_f32("3,14"), None);
        assert_eq!(parse_f32("inf"), None);
        assert_eq!(parse_f32("1e3"), None);
    }

    #[test]
    fn field_texts() {
        assert_eq!(pitch_field_text(3.0), "+3.00");
        assert_eq!(pitch_field_text(-0.07), "-0.07");
        assert_eq!(tract_field_text(170.0), "170");
        assert_eq!(air_field_text(-30), "-12.0");
    }

    #[test]
    fn eq_texts_are_canonical() {
        assert_eq!(eq_freq_text(100.0), "100 Hz");
        assert_eq!(eq_freq_text(350.4), "350 Hz");
        assert_eq!(eq_freq_text(3_000.0), "3.00 kHz");
        assert_eq!(eq_freq_text(20_000.0), "20.00 kHz");
        assert_eq!(eq_q_text(0.707), "0.71");
        assert_eq!(eq_gain_text(2.5), "+2.5 dB");
        assert_eq!(eq_gain_text(-3.0), "-3.0 dB");
        assert_eq!(eq_gain_text(0.0), "+0.0 dB");
    }

    #[test]
    fn comp_texts_are_canonical() {
        assert_eq!(comp_threshold_text(-20.0), "-20.0 dB");
        assert_eq!(comp_threshold_text(0.0), "0.0 dB");
        assert_eq!(comp_ratio_text(3.5), "3.5:1");
        assert_eq!(comp_ratio_text(1.0), "1.0:1");
        assert_eq!(comp_ms_text(10.0), "10.0 ms");
        assert_eq!(comp_ms_text(2_000.0), "2000.0 ms");
        assert_eq!(comp_knee_text(6.0), "6.0 dB");
        assert_eq!(comp_makeup_text(2.5), "+2.5 dB");
        assert_eq!(comp_makeup_text(0.0), "+0.0 dB");
        assert_eq!(comp_mix_text(100.0), "100 %");
        assert_eq!(comp_mix_text(30.0), "30 %");
        assert_eq!(comp_gr_text(-3.24), "-3.2 dB");
        assert_eq!(comp_gr_text(0.0), "0.0 dB");
        assert_eq!(comp_gr_text(1.5), "0.0 dB", "GR never reads positive");
        assert_eq!(comp_gr_text(f32::NAN), "0.0 dB");
    }
}
