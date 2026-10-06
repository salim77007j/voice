//! Headless UI integration tests: the real `.slint` component tree, real
//! controller wiring, real translations — rendered and inspected without
//! a display (the anti-fake-UI rule's strongest enforcement).
//!
//! These tests drive the same callbacks the widgets invoke, so a broken
//! binding or a dead button fails here even though no window ever opens.

use mvl_app::controller::Controller;
use mvl_app::headless;
use mvl_core::VocalParams;
use mvl_io::InterleavedAudio;
use slint::{ComponentHandle, Model, SharedString};

/// 1.0 s of voice-like material (harmonic stack + envelope).
fn vocal_like() -> InterleavedAudio {
    let rate = 48_000u32;
    let data = (0..48_000)
        .map(|i| {
            let t = i as f64 / f64::from(rate);
            let env = (-t * 0.5).exp() * (1.0 - (-t * 6.0).exp());
            let f0 = 196.0 + 3.0 * (2.0 * std::f64::consts::PI * 5.0 * t).sin();
            let v = (2.0 * std::f64::consts::PI * f0 * t).sin() * 0.45
                + (2.0 * std::f64::consts::PI * 2.0 * f0 * t).sin() * 0.18
                + (2.0 * std::f64::consts::PI * 3.0 * f0 * t).sin() * 0.09
                + (2.0 * std::f64::consts::PI * 4.0 * f0 * t).sin() * 0.05;
            (v * env * 0.8) as f32
        })
        .collect();
    InterleavedAudio::new(data, rate, 1).unwrap()
}

fn make_controller() -> Controller {
    headless::install();
    let app = mvl_app::AppWindow::new().expect("headless component");
    Controller::new(app)
}

/// Blue-dominant pixel (waveform trace blended over the dark panel).
fn is_blue(p: (u8, u8, u8)) -> bool {
    i32::from(p.2) > 90 && i32::from(p.2) > i32::from(p.0) + 30
}

#[test]
fn initial_state_is_empty_and_english() {
    let c = make_controller();
    let app = c.window();
    assert!(!app.get_has_audio());
    assert_eq!(app.get_status_key(), "no-audio");
    assert_eq!(app.get_pitch_semitones(), 0.0);
    assert_eq!(app.get_air_percent(), 0);
    assert!((app.get_tract_mm() - 170.0).abs() < 1e-6);
    assert_eq!(app.get_pitch_value_text(), "+0.00");
    assert_eq!(app.get_air_value_text(), "+0.0");
    assert_eq!(app.get_tract_value_text(), "170");
    // neutral formant detail
    assert_eq!(app.get_tract_detail_text(), "interval +0.00 st");
    // LTR by default
    assert!(!app.global::<mvl_app::Translations>().get_rtl());
}

#[test]
fn loading_audio_populates_session_view() {
    let c = make_controller();
    c.load_audio_sync(vocal_like(), "take-one".into());
    let app = c.window();
    assert!(app.get_has_audio());
    assert_eq!(app.get_status_key(), "audio-loaded");
    assert_eq!(app.get_duration_text(), "00:00:01.000");
    assert_eq!(app.get_rate_text(), "48000 Hz · 1 ch");
    // ruler ticks were produced for the full view
    assert!(!app.get_ticks().iter().next().is_none());
    // the waveform image was rendered at a real size
    let wave = app.get_wave();
    assert!(wave.size().width > 0, "waveform image must exist");
}

#[test]
fn param_changes_flow_through_sanitized() {
    let c = make_controller();
    let app = c.window();
    // slider-style change (the callback path the widget uses)
    app.invoke_set_pitch(3.07);
    assert!((app.get_pitch_semitones() - 3.07).abs() < 1e-6);
    assert_eq!(app.get_pitch_value_text(), "+3.07");
    assert_eq!(app.get_pitch_detail_text(), "C 5 +7¢");
    // clamping via the text commit path
    app.invoke_commit_pitch_text("99".into());
    assert!(
        (app.get_pitch_semitones() - 12.0).abs() < 1e-6,
        "must clamp to +12"
    );
    // invalid text restores the canonical value
    app.invoke_commit_pitch_text("not-a-number".into());
    assert!((app.get_pitch_semitones() - 12.0).abs() < 1e-6);
    assert_eq!(app.get_pitch_value_text(), "+12.00");
    // air: −12 dB readout = −30 % (plan §8.3 example)
    app.invoke_commit_air_text("-12".into());
    assert_eq!(app.get_air_percent(), -30);
    assert_eq!(app.get_air_value_text(), "-12.0");
    // tract: mm text commit
    app.invoke_commit_tract_text("140".into());
    assert!((app.get_tract_mm() - 140.0).abs() < 1e-6);
    assert!(app.get_tract_detail_text().starts_with("interval +"));
    // resets return each module to neutral
    app.invoke_reset_pitch();
    app.invoke_reset_air();
    app.invoke_reset_tract();
    assert_eq!(app.get_pitch_semitones(), 0.0);
    assert_eq!(app.get_air_percent(), 0);
    assert!((app.get_tract_mm() - 170.0).abs() < 1e-6);
}

#[test]
fn zoom_and_view_state_tracks_session() {
    let c = make_controller();
    c.load_audio_sync(vocal_like(), "take".into());
    let app = c.window();
    // full view of a 1 s file
    assert!(app.get_view_start().abs() < 1e-6);
    assert!((app.get_view_end() - 1.0).abs() < 1e-6);
    assert!(!app.get_sample_level());
    // zoom in hard → the span shrinks and sample level engages eventually
    for _ in 0..24 {
        app.invoke_zoom_at(0.5, 1);
    }
    let span = f64::from(app.get_view_end() - app.get_view_start());
    assert!(span < 0.01, "span {span} must shrink");
    assert!(app.get_sample_level(), "deep zoom must reach sample level");
    // fit restores the full file
    app.invoke_zoom_fit_clicked();
    assert!((f64::from(app.get_view_end() - app.get_view_start()) - 1.0).abs() < 1e-6);
    // selection callbacks publish the range
    app.invoke_select_seconds(0.25, 0.75);
    assert!(app.get_has_selection());
    assert!((app.get_sel_start() - 0.25).abs() < 1e-6);
    assert!((app.get_sel_end() - 0.75).abs() < 1e-6);
}

#[test]
fn language_switch_flips_rtl_and_translates() {
    let c = make_controller();
    c.load_audio_sync(vocal_like(), "take".into());
    let app = c.window();
    assert_eq!(app.get_status_key(), "audio-loaded");

    // switch to Arabic via the same callback the عربي button invokes
    app.invoke_set_language("ar".into());
    assert!(app.global::<mvl_app::Translations>().get_rtl());
    assert_eq!(
        app.global::<mvl_app::Translations>().get_locale(),
        SharedString::from("ar")
    );

    // back to English
    app.invoke_set_language("en".into());
    assert!(!app.global::<mvl_app::Translations>().get_rtl());
}

#[test]
fn play_without_device_reports_honestly() {
    // In this container there is no audio output device; the transport
    // must surface that as a status instead of pretending to play.
    let c = make_controller();
    c.load_audio_sync(vocal_like(), "take".into());
    let app = c.window();
    app.invoke_play_pause_clicked();
    // status is either untouched (a real device exists and playback
    // started) or the honest no-device error — never a silent lie.
    let key = app.get_status_key().to_string();
    assert!(
        key == "audio-loaded" || key == "no-output-device" || key == "export-error",
        "unexpected status {key}"
    );
}

#[test]
fn screenshot_renders_real_pixels_en_and_ar() {
    let dir = std::env::temp_dir().join("mvl-app-ui-tests");
    std::fs::create_dir_all(&dir).unwrap();

    // English / neutral / loaded audio
    {
        let c = make_controller();
        c.load_audio_sync(vocal_like(), "take-one".into());
        c.set_params(VocalParams {
            pitch_semitones: 3.0,
            air_percent: -30,
            tract_mm: 140.0,
            ..VocalParams::neutral()
        });
        let app = c.window();
        app.set_playhead(0.4);
        app.set_show_playhead(true);
        // One honest UI tick: the analysis rack must publish real data
        // (40 RTA bands + peak caps + L/R levels) for the playhead window.
        c.refresh();
        let bands = app.get_spectrum_bands();
        assert_eq!(bands.iter().count(), 40, "RTA model must hold 40 bands");
        let lit = bands.iter().filter(|&b| b > 0.01).count();
        assert!(
            lit >= 3,
            "voiced material at the playhead must light RTA bands (got {lit})"
        );
        assert!(app.get_level_l() > 0.0, "master meter must register signal");
        assert!(!app.get_clip_latch(), "-6 dBFS material must not clip");
        let out = dir.join("ui-en.png");
        headless::render_to_png(app, slint::PhysicalSize::new(1280, 1060), &out).unwrap();
        assert!(
            out.exists() && out.metadata().unwrap().len() > 20_000,
            "PNG must be written with real content"
        );

        // pixel truth: the Phase 7.2 rack design tokens must be rendered.
        // Layout at 1280×1060 (two post-chain rack rows: EQ + compressor,
        // Phase 8.4): 6px outer padding (bg-base #1A1D21), then nameplate
        // (52px), waveform rack (stretch), EQ (196px), compressor (196px),
        // main row (292px), transport (60px), status bar (26px) — the
        // waveform gets ≈190px of stretch, mid ≈ y 159; the status strip
        // centers ≈ y 1049.
        let buf = headless::render_to_buffer(app, slint::PhysicalSize::new(1280, 1060));
        assert_eq!((buf.width(), buf.height()), (1280, 1060));
        let px = buf.as_bytes();
        let at = |x: u32, y: u32| {
            let o = ((y * 1280 + x) * 3) as usize;
            (px[o], px[o + 1], px[o + 2])
        };
        // outer padding is app background #1A1D21 at the very corner
        assert_eq!(at(4, 4), (26, 29, 33), "corner must be bg-base #1A1D21");
        // status bar interior is #1E2126 (bottom strip)
        let status = at(640, 1049);
        assert!(
            (status.0 as i32 - 30).abs() <= 2
                && (status.1 as i32 - 33).abs() <= 2
                && (status.2 as i32 - 38).abs() <= 2,
            "status bar must be #1E2126, got {status:?}"
        );
        // waveform rack interior: panel #242729, the blue trace #4A90E2,
        // or the gradient RMS body (alpha 60..190 over panel) — a
        // blue-dominant pixel in all cases, never a stray colour.
        let mid = at(640, 159);
        assert!(
            mid == (36, 39, 41) || mid == (74, 144, 226) || is_blue(mid),
            "waveform canvas must be panel or trace-coloured, got {mid:?}"
        );
        // gold accent #E6B800 must exist (fader fills, knob indicators —
        // solid rectangles, not antialiased text)
        let gold = px
            .chunks_exact(3)
            .any(|p| p[0] == 230 && p[1] == 184 && p[2] == 0);
        assert!(gold, "accent #E6B800 must appear in the render");
        // blue audio-data trace must exist
        let blue = px
            .chunks_exact(3)
            .any(|p| p[0] == 74 && p[1] == 144 && p[2] == 226);
        assert!(blue, "waveform blue #4A90E2 must appear in the render");
    }

    // Arabic / RTL
    {
        let c = make_controller();
        c.load_audio_sync(vocal_like(), "take-one".into());
        let app = c.window();
        app.invoke_set_language("ar".into());
        let out = dir.join("ui-ar.png");
        headless::render_to_png(app, slint::PhysicalSize::new(1280, 1060), &out).unwrap();
        assert!(out.exists() && out.metadata().unwrap().len() > 20_000);
        let buf = headless::render_to_buffer(app, slint::PhysicalSize::new(1280, 1060));
        let px = buf.as_bytes();
        let at = |x: u32, y: u32| {
            let o = ((y * 1280 + x) * 3) as usize;
            (px[o], px[o + 1], px[o + 2])
        };
        // outer padding still dominates the corner in RTL
        assert_eq!(at(4, 4), (26, 29, 33));
    }
}
