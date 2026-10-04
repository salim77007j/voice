//! On-device verification kit (Phase 5, plan §14: "on-device verification
//! with screenshots").
//!
//! `micro-vocal-lab selftest [OPTIONS]` runs the *real* hardware paths end
//! to end on a human's machine — the default input device, the default
//! output device, the live preview engine, the offline export engine and
//! the real UI component tree — and collects the evidence files a phase
//! report needs into one directory:
//!
//! ```text
//! recording.wav       the take captured from the real input device
//! screenshot-en.png   the real UI (software renderer, same component tree)
//! screenshot-ar.png   … in Arabic + RTL
//! exported.wav/.mp3   offline Render-profile output of the same take
//! report.txt          the PASS/FAIL/SKIP table below
//! ```
//!
//! What it proves mechanically: device enumeration + 192 kHz negotiation
//! (with an honest fallback note when the device caps lower), disk
//! streaming, re-import, wall-clock-accurate playback through the DSP
//! engine, offline export in both codecs, EN/AR UI renders.
//! What it cannot prove: perception (does it *sound* natural? does the
//! window *look* right? do the native dialogs open?). That is the human
//! checklist in `docs/ONDEVICE.md` — run this selftest first, then walk
//! the checklist while listening.

use crate::controller::Controller;
use crate::headless;
use mvl_core::{QualityProfile, VocalEngine, VocalParams};
use mvl_io::recorder::{default_host_name, default_input_device_name, RecordRequest, Recorder};
use mvl_io::{resample, InterleavedAudio};
use std::path::Path;
use std::time::{Duration, Instant};

/// Rate of the real-time preview path (mirrors `session::PREVIEW_RATE`).
const PREVIEW_RATE: u32 = 48_000;

/// The parameters the selftest audibly demonstrates. Non-neutral on
/// purpose: the operator should *hear* the pitch move and the breath duck.
fn selftest_params() -> VocalParams {
    VocalParams {
        pitch_semitones: 3.0,
        air_percent: -30,
        tract_mm: 140.0,
    }
}

/// Exit code semantics: 0 = no FAIL (SKIP is acceptable — e.g. a machine
/// with no output device), 1 = at least one FAIL.
pub fn run(seconds: u32, out_dir: &Path) -> i32 {
    let mut report = Report::new(seconds, out_dir);

    println!(
        "Micro-Vocal Lab {} — on-device selftest",
        env!("CARGO_PKG_VERSION")
    );
    println!("output directory: {}", out_dir.display());
    println!();
    println!("  audio host:   {}", default_host_name());
    match default_input_device_name() {
        Ok(Some(name)) => println!("  input device: {name}"),
        Ok(None) => println!("  input device: none detected"),
        Err(e) => println!("  input device: enumeration failed ({e})"),
    }
    println!();

    // --- 1. capture from the real input device -------------------------
    println!("[1/5] recording {seconds} s from the default input — SPEAK INTO THE MICROPHONE …");
    let take = match record_step(&mut report, seconds, out_dir) {
        Some(t) => t,
        None => {
            report.skip_rest("capture failed — later steps need audio");
            return report.finish(out_dir);
        }
    };

    // --- 2. re-import --------------------------------------------------
    println!("[2/5] re-importing the recorded WAV …");
    reimport_step(&mut report, &take, out_dir);

    // --- 3. live preview through the engine -----------------------------
    println!("[3/5] playing the take through the live engine — LISTEN: pitch +3 st, air −30 %, tract 140 mm …");
    preview_step(&mut report, &take);

    // --- 4. offline export ---------------------------------------------
    println!("[4/5] offline Render-profile export (WAV + MP3) …");
    export_step(&mut report, &take, out_dir);

    // --- 5. UI evidence -------------------------------------------------
    println!("[5/5] rendering EN + AR (RTL) screenshots of the loaded take …");
    screenshot_step(&mut report, &take, out_dir);

    report.finish(out_dir)
}

// ---------------------------------------------------------------------------
// steps
// ---------------------------------------------------------------------------

/// Records `seconds` s from the default input device into
/// `out_dir/recording.wav`; returns the imported take.
fn record_step(report: &mut Report, seconds: u32, out_dir: &Path) -> Option<InterleavedAudio> {
    let path = out_dir.join("recording.wav");
    let requested = RecordRequest::default();
    let recorder = match Recorder::start(&path, requested) {
        Ok(r) => r,
        Err(e) => {
            report.push("capture", Outcome::Fail, format!("open default input: {e}"));
            return None;
        }
    };
    let cfg = recorder.actual_config();
    if cfg.sample_rate < requested.target_rate {
        report.push(
            "capture",
            Outcome::Pass,
            format!(
                "{} Hz × {} ch (device capped below the {} Hz studio target — honest fallback, R4)",
                cfg.sample_rate, cfg.channels, requested.target_rate
            ),
        );
    } else {
        report.push(
            "capture",
            Outcome::Pass,
            format!(
                "{} Hz × {} ch (studio target)",
                cfg.sample_rate, cfg.channels
            ),
        );
    }
    std::thread::sleep(Duration::from_secs(u64::from(seconds)));
    let stats = match recorder.stop() {
        Ok(s) => s,
        Err(e) => {
            report.push("capture", Outcome::Fail, format!("stop/finalize: {e}"));
            return None;
        }
    };
    if stats.dropped_samples > 0 {
        report.push(
            "capture",
            Outcome::Fail,
            format!(
                "{} samples dropped (I/O stall > ring capacity)",
                stats.dropped_samples
            ),
        );
        return None;
    }
    let expected = f64::from(cfg.sample_rate) * f64::from(seconds);
    let dev = (stats.frames as f64 - expected).abs() / expected;
    if dev > 0.15 {
        report.push(
            "capture",
            Outcome::Fail,
            format!(
                "duration off by {dev:.0} % ({} frames ≈ {:.2} s, asked {seconds} s)",
                stats.frames,
                stats.frames as f64 / f64::from(cfg.sample_rate)
            ),
        );
        return None;
    }
    match mvl_io::import(&path) {
        Ok(audio) => {
            let peak = audio.data.iter().fold(0.0f32, |a, &s| a.max(s.abs()));
            let db = if peak > 0.0 {
                20.0 * peak.log10()
            } else {
                f32::NEG_INFINITY
            };
            let note = if db < -60.0 {
                format!(
                    "input nearly silent (peak {db:.1} dBFS) — was the mic muted? evidence kept"
                )
            } else {
                format!("peak {db:.1} dBFS")
            };
            report.push("capture-file", Outcome::Pass, note);
            Some(audio)
        }
        Err(e) => {
            report.push(
                "capture-file",
                Outcome::Fail,
                format!("re-open recording.wav: {e}"),
            );
            None
        }
    }
}

fn reimport_step(report: &mut Report, take: &InterleavedAudio, _out_dir: &Path) {
    // `take` IS the re-imported file — the import call already succeeded in
    // `record_step`; here we verify the content is sane.
    let dur = take.duration_seconds();
    if take.data.iter().all(|s| s.is_finite()) && dur > 0.0 {
        report.push(
            "re-import",
            Outcome::Pass,
            format!("{dur:.2} s, {} ch, finite samples", take.channels),
        );
    } else {
        report.push(
            "re-import",
            Outcome::Fail,
            format!("bad content (dur {dur:.2} s)"),
        );
    }
}

fn preview_step(report: &mut Report, take: &InterleavedAudio) {
    let preview = match resample::resample(take, PREVIEW_RATE) {
        Ok(p) => p,
        Err(e) => {
            report.push(
                "live preview",
                Outcome::Fail,
                format!("resample to preview rate: {e}"),
            );
            return;
        }
    };
    let player = match crate::preview::PreviewPlayer::new(&preview) {
        Ok(p) => p,
        Err(e) => {
            report.push(
                "live preview",
                Outcome::Skip,
                format!("no usable output device ({e})"),
            );
            return;
        }
    };
    if let Err(e) = player.set_params(selftest_params()) {
        report.push("live preview", Outcome::Fail, format!("set params: {e}"));
        return;
    }
    let listen = Duration::from_millis(3_000);
    let started = Instant::now();
    if let Err(e) = player.play() {
        report.push("live preview", Outcome::Fail, format!("play: {e}"));
        return;
    }
    std::thread::sleep(listen);
    let pos = player.position_seconds();
    let _ = player.pause();
    let wall = started.elapsed().as_secs_f64();
    // A real (clock-paced) device advances roughly with wall time.
    if pos >= 1.0 && pos <= wall + 1.5 {
        report.push(
            "live preview",
            Outcome::Pass,
            format!("engine-fed playback advanced {pos:.2} s in {wall:.2} s wall clock"),
        );
    } else {
        report.push(
            "live preview",
            Outcome::Fail,
            format!("position {pos:.2} s after {wall:.2} s — output device not pacing?"),
        );
    }
}

fn export_step(report: &mut Report, take: &InterleavedAudio, out_dir: &Path) {
    let preview = match resample::resample(take, PREVIEW_RATE) {
        Ok(p) => p,
        Err(e) => {
            report.push("export", Outcome::Fail, format!("resample: {e}"));
            return;
        }
    };
    let started = Instant::now();
    let rendered = match VocalEngine::render_interleaved(
        &preview.data,
        preview.channels,
        preview.sample_rate,
        selftest_params(),
        QualityProfile::Render,
    ) {
        Ok(r) => r,
        Err(e) => {
            report.push("export", Outcome::Fail, format!("engine render: {e}"));
            return;
        }
    };
    let elapsed = started.elapsed().as_secs_f64();
    let factor = preview.duration_seconds() / elapsed.max(1e-9);
    if factor < 1.0 {
        report.push(
            "export",
            Outcome::Fail,
            format!("render slower than realtime ({factor:.1}×)"),
        );
        return;
    }
    let note = if factor < 10.0 {
        format!("{factor:.1}× realtime (below the 10× budget, §9 — machine under load?)")
    } else {
        format!("{factor:.1}× realtime")
    };
    let out = match InterleavedAudio::new(rendered, preview.sample_rate, preview.channels) {
        Ok(o) => o,
        Err(e) => {
            report.push("export", Outcome::Fail, format!("assemble output: {e}"));
            return;
        }
    };
    match mvl_io::wav::export(
        &out_dir.join("exported.wav"),
        &out,
        mvl_io::WavDepth::Float32,
    ) {
        Ok(()) => report.push("export WAV", Outcome::Pass, note),
        Err(e) => report.push("export WAV", Outcome::Fail, format!("{e}")),
    }
    match mvl_io::mp3::export(
        &out_dir.join("exported.mp3"),
        &out,
        &mvl_io::Mp3Settings::default(),
    ) {
        Ok(()) => report.push("export MP3", Outcome::Pass, "LAME encode ok".into()),
        Err(e) => report.push("export MP3", Outcome::Fail, format!("{e}")),
    }
}

fn screenshot_step(report: &mut Report, take: &InterleavedAudio, out_dir: &Path) {
    let preview = match resample::resample(take, PREVIEW_RATE) {
        Ok(p) => p,
        Err(e) => {
            report.push("UI screenshots", Outcome::Fail, format!("resample: {e}"));
            return;
        }
    };
    headless::install();
    for (locale, name) in [("en", "screenshot-en.png"), ("ar", "screenshot-ar.png")] {
        let app = match crate::AppWindow::new() {
            Ok(a) => a,
            Err(e) => {
                report.push(
                    "UI screenshots",
                    Outcome::Fail,
                    format!("component build: {e}"),
                );
                return;
            }
        };
        let controller = Controller::new(app);
        controller.load_audio_sync(preview.clone(), "selftest take".into());
        controller.set_params(selftest_params());
        if locale == "ar" {
            controller.window().invoke_set_language("ar".into());
        }
        let size = slint::PhysicalSize::new(1280, 800);
        let path = out_dir.join(name);
        match headless::render_to_png(controller.window(), size, &path) {
            Ok(()) => report.push(
                "UI screenshots",
                Outcome::Pass,
                format!("{name} (locale {locale})"),
            ),
            Err(e) => report.push("UI screenshots", Outcome::Fail, format!("{name}: {e}")),
        }
    }
}

// ---------------------------------------------------------------------------
// reporting
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Pass,
    Fail,
    Skip,
}

impl Outcome {
    fn label(self) -> &'static str {
        match self {
            Outcome::Pass => "PASS",
            Outcome::Fail => "FAIL",
            Outcome::Skip => "SKIP",
        }
    }
}

struct Report {
    seconds: u32,
    steps: Vec<(String, Outcome, String)>,
}

impl Report {
    fn new(seconds: u32, _out_dir: &Path) -> Self {
        Self {
            seconds,
            steps: Vec::new(),
        }
    }

    fn push(&mut self, name: &str, outcome: Outcome, note: String) {
        println!("  {} {:<16} {}", outcome.label(), name, note);
        self.steps.push((name.to_string(), outcome, note));
    }

    fn skip_rest(&mut self, reason: &str) {
        for name in [
            "re-import",
            "live preview",
            "export WAV",
            "export MP3",
            "UI screenshots",
        ] {
            self.steps.push((name.into(), Outcome::Skip, reason.into()));
        }
    }

    fn finish(&mut self, out_dir: &Path) -> i32 {
        let (pass, fail, skip) = self.tally();
        let verdict = if fail == 0 {
            "OK (no failures)"
        } else {
            "FAILED"
        };
        let mut text = String::new();
        text.push_str(&format!(
            "Micro-Vocal Lab on-device selftest — {} s take\n\
             date: {}  ·  {} {}  ·  audio host: {}\n\n",
            self.seconds,
            chrono_like_now(),
            std::env::consts::OS,
            std::env::consts::ARCH,
            default_host_name(),
        ));
        for (name, outcome, note) in &self.steps {
            text.push_str(&format!("  {:<6} {:<16} {}\n", outcome.label(), name, note));
        }
        text.push_str(&format!(
            "\nverdict: {verdict} — {pass} pass, {fail} fail, {skip} skip\n"
        ));
        let path = out_dir.join("report.txt");
        if let Err(e) = std::fs::write(&path, &text) {
            eprintln!("warning: could not write {}: {e}", path.display());
        }
        println!("\nverdict: {verdict} — {pass} pass / {fail} fail / {skip} skip");
        println!("report:  {}", path.display());
        if fail == 0 {
            0
        } else {
            1
        }
    }

    fn tally(&self) -> (usize, usize, usize) {
        let pass = self
            .steps
            .iter()
            .filter(|(_, o, _)| *o == Outcome::Pass)
            .count();
        let fail = self
            .steps
            .iter()
            .filter(|(_, o, _)| *o == Outcome::Fail)
            .count();
        let skip = self
            .steps
            .iter()
            .filter(|(_, o, _)| *o == Outcome::Skip)
            .count();
        (pass, fail, skip)
    }
}

/// RFC-3339-ish local timestamp without pulling in a chrono dependency
/// into the app crate (the release build stays lean, Phase 5 §2).
fn chrono_like_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Days since epoch -> civil date (Howard Hinnant's algorithm).
    let days = (secs / 86_400) as i64;
    let (y, m, d) = civil_from_days(days);
    let rem = secs % 86_400;
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_date_matches_known_values() {
        // 2026-10-04 is day 20,730 since the epoch.
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_730), (2026, 10, 4));
    }

    #[test]
    fn params_are_non_neutral_and_in_range() {
        let p = selftest_params();
        let s = p.sanitized();
        assert_eq!(s.pitch_semitones, 3.0, "pitch must stay +3 st");
        assert_eq!(s.air_percent, -30, "air must stay −30 %");
        assert_eq!(s.tract_mm, 140.0, "tract must stay 140 mm");
    }

    #[test]
    fn outcome_labels_are_stable_for_the_report() {
        assert_eq!(Outcome::Pass.label(), "PASS");
        assert_eq!(Outcome::Fail.label(), "FAIL");
        assert_eq!(Outcome::Skip.label(), "SKIP");
    }
}
