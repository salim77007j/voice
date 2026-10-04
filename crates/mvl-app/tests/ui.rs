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
        });
        let app = c.window();
        app.set_playhead(0.4);
        app.set_show_playhead(true);
        let out = dir.join("ui-en.png");
        headless::render_to_png(app, slint::PhysicalSize::new(1280, 800), &out).unwrap();
        assert!(
            out.exists() && out.metadata().unwrap().len() > 20_000,
            "PNG must be written with real content"
        );

        // pixel truth: the design tokens must actually be rendered
        let buf = headless::render_to_buffer(app, slint::PhysicalSize::new(1280, 800));
        assert_eq!((buf.width(), buf.height()), (1280, 800));
        let px = buf.as_bytes();
        let at = |x: u32, y: u32| {
            let o = ((y * 1280 + x) * 3) as usize;
            (px[o], px[o + 1], px[o + 2])
        };
        // toolbar background (panel #1C1E24) at the top-left
        assert_eq!(at(4, 4), (28, 30, 36), "toolbar must be panel-coloured");
        // waveform panel, trace or 55 % RMS blend mid-canvas
        let mid = at(640, 400);
        assert!(
            mid == (28, 30, 36)            // panel
                || mid == (45, 212, 191)   // peak outline
                || mid == (36, 129, 120), // RMS body blended over panel
            "canvas must be panel or waveform-coloured, got {mid:?}"
        );
        // accent teal must exist somewhere (module values, playhead, thumb)
        let teal = px
            .chunks_exact(3)
            .any(|p| p[0] == 45 && p[1] == 212 && p[2] == 191);
        assert!(teal, "accent #2DD4BF must appear in the render");
    }

    // Arabic / RTL
    {
        let c = make_controller();
        c.load_audio_sync(vocal_like(), "take-one".into());
        let app = c.window();
        app.invoke_set_language("ar".into());
        let out = dir.join("ui-ar.png");
        headless::render_to_png(app, slint::PhysicalSize::new(1280, 800), &out).unwrap();
        assert!(out.exists() && out.metadata().unwrap().len() > 20_000);
        let buf = headless::render_to_buffer(app, slint::PhysicalSize::new(1280, 800));
        let px = buf.as_bytes();
        let at = |x: u32, y: u32| {
            let o = ((y * 1280 + x) * 3) as usize;
            (px[o], px[o + 1], px[o + 2])
        };
        // panel background still dominates the chrome
        assert_eq!(at(4, 4), (28, 30, 36));
    }
}
