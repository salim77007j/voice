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
}
