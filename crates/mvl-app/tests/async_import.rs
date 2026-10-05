//! Regression tests for the v1.1.1 BUG 2 fix: the async import path
//! (worker thread → inbox → `ui_tick` drain → `install_session`).
//!
//! The reported Windows crash was
//! `panicked at controller.rs:977: RefCell already borrowed` on every
//! import through the dialog/CLI. Root cause: the inbox drain loop held
//! a `Ref` (from `state.borrow().inbox.try_recv()` in the `while let`
//! scrutinee) across the whole loop body, so `install_session`'s
//! `borrow_mut()` re-entered the same cell. These tests drive the exact
//! user flow — `load_path` (worker + inbox), then `refresh()` (the same
//! drain the 30 fps timer runs) — so the double-borrow, if it ever
//! comes back, panics here instead of on a user's machine.

use mvl_app::controller::Controller;
use mvl_app::headless;

/// Write a small valid mono 48 kHz float32 WAV and return its path.
fn write_wav(dir: &std::path::Path, name: &str, seconds: f64) -> std::path::PathBuf {
    let path = dir.join(name);
    let rate = 48_000u32;
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut w = hound::WavWriter::create(&path, spec).unwrap();
    let n = (f64::from(rate) * seconds) as usize;
    for i in 0..n {
        let t = i as f64 / f64::from(rate);
        let v = (2.0 * std::f64::consts::PI * 196.0 * t).sin() * 0.4;
        w.write_sample(v as f32).unwrap();
    }
    w.finalize().unwrap();
    path
}

fn make_controller() -> Controller {
    headless::install();
    let app = mvl_app::AppWindow::new().expect("headless component");
    Controller::new(app)
}

/// Pump the same drain the 30 fps UI timer runs until `cond` holds
/// (or the deadline passes). Returns false on timeout.
fn pump_until(c: &Controller, mut cond: impl FnMut() -> bool) -> bool {
    for _ in 0..400 {
        // The exact code the timer callback executes.
        c.refresh();
        if cond() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    false
}

#[test]
fn async_import_installs_session_without_refcell_panic() {
    let dir = std::env::temp_dir();
    let wav = write_wav(&dir, "mvl-regression-async-import.wav", 0.25);

    let c = make_controller();
    // The user flow: file → worker thread → SessionReady in the inbox.
    c.load_path(wav.clone());
    let app = c.window();
    // Drain exactly like the 30 fps timer. Before the v1.1.1 fix this
    // panicked with "RefCell already borrowed" inside install_session
    // the moment the worker's message landed.
    let installed = pump_until(&c, || {
        app.get_has_audio() || app.get_status_key() == "import-error"
    });
    assert!(installed, "worker result must arrive within the deadline");
    assert!(
        app.get_has_audio(),
        "valid WAV must install; status key = {}",
        app.get_status_key()
    );
    assert_eq!(app.get_status_key(), "audio-loaded");
    assert_eq!(app.get_duration_text(), "00:00:00.250");
    assert!(!app.get_busy(), "busy flag must clear after install");
    let _ = std::fs::remove_file(&wav);
}

#[test]
fn async_import_error_surfaces_in_status_bar() {
    let dir = std::env::temp_dir();
    let garbage = dir.join("mvl-regression-garbage.wav");
    std::fs::write(&garbage, b"this is not audio data at all").unwrap();

    let c = make_controller();
    c.load_path(garbage.clone());
    let app = c.window();
    let done = pump_until(&c, || app.get_status_key() == "import-error");
    assert!(
        done,
        "typed error must arrive; got {}",
        app.get_status_key()
    );
    assert!(
        !app.get_has_audio(),
        "failed import must not install a session"
    );
    assert!(
        !app.get_busy(),
        "busy flag must clear after a failed import"
    );
    let _ = std::fs::remove_file(&garbage);
}

#[test]
fn async_import_twice_in_sequence_reuses_the_drain() {
    // Two sequential imports exercise repeated inbox cycles: install →
    // busy cleared → second worker → second install. A stuck busy flag
    // or a re-entrant borrow in the second cycle fails here.
    let dir = std::env::temp_dir();
    let a = write_wav(&dir, "mvl-regression-seq-a.wav", 0.1);
    let b = write_wav(&dir, "mvl-regression-seq-b.wav", 0.2);

    let c = make_controller();
    let app = c.window();

    c.load_path(a.clone());
    assert!(pump_until(&c, || app.get_has_audio()), "first import");
    assert_eq!(app.get_status_key(), "audio-loaded");

    c.load_path(b.clone());
    assert!(
        pump_until(&c, || app.get_status_key() == "audio-loaded"
            && app.get_file_name().to_string().contains("seq-b")),
        "second import must install (busy flag must have cleared)"
    );

    let _ = std::fs::remove_file(&a);
    let _ = std::fs::remove_file(&b);
}

#[test]
fn rapid_refresh_burst_while_worker_runs_is_safe() {
    // Hammer refresh() in a tight loop from before the worker starts to
    // after its message lands — the drain must tolerate being entered
    // at any phase, including with a message already queued.
    let dir = std::env::temp_dir();
    let wav = write_wav(&dir, "mvl-regression-burst.wav", 0.1);
    let c = make_controller();
    c.load_path(wav.clone());
    let app = c.window();
    for _ in 0..200 {
        c.refresh();
        if app.get_has_audio() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    assert!(
        app.get_has_audio(),
        "burst polling must install the session"
    );
    let _ = std::fs::remove_file(&wav);
}
