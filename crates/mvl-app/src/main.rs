//! Micro-Vocal Lab application binary.
//!
//! Subcommands:
//! * *(default)* `run [file]` — the desktop UI (winit + femtovg)
//! * `process <in> <out> [options]` — headless DSP render (Phase 3 CLI)
//! * `screenshot <out.png> [file] [options]` — offscreen UI render for
//!   verification (the same UI code, software renderer, no display needed)
//! * `selftest [--seconds N] [DIR]` — on-device verification kit (Phase 5):
//!   records from the real input, plays through the real engine, exports,
//!   renders EN/AR screenshots, writes a PASS/FAIL report
//! * `self-check` — boot diagnostics

use mvl_app::{controller::Controller, headless};
use mvl_core::{QualityProfile, VocalEngine, VocalParams};
use mvl_io::{InterleavedAudio, WavDepth};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("process") => process_command(&args[2..]),
        Some("screenshot") => screenshot_command(&args[2..]),
        Some("selftest") => selftest_command(&args[2..]),
        Some("self-check") => self_check(),
        Some("run") => run_command(&args[2..]),
        Some("--help") | Some("-h") | Some("help") => usage(),
        _ => run_command(&args[1..]),
    }
}

fn usage() {
    println!(
        "Micro-Vocal Lab {}\n\
         \n\
         USAGE:\n\
             micro-vocal-lab [run] [FILE]      desktop UI (optionally loads FILE)\n\
             micro-vocal-lab process IN OUT [--pitch ST] [--air %] [--tract MM] [--profile preview|render]\n\
             micro-vocal-lab screenshot OUT.png [FILE] [--pitch ST] [--air %] [--tract MM]\n\
                                            [--locale en|ar] [--width PX] [--height PX] [--playhead SEC]\n\
             micro-vocal-lab selftest [--seconds N] [DIR]\n\
             micro-vocal-lab self-check",
        env!("CARGO_PKG_VERSION")
    );
}

// ---------------------------------------------------------------------------
// run — the desktop UI
// ---------------------------------------------------------------------------

fn run_command(args: &[String]) {
    let file = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .map(std::path::PathBuf::from);

    let app = match mvl_app::AppWindow::new() {
        Ok(app) => app,
        Err(e) => {
            eprintln!("error: cannot open a window on this machine: {e}");
            eprintln!("hint: for headless verification use `micro-vocal-lab screenshot out.png`");
            std::process::exit(1);
        }
    };
    let controller = Controller::new(app);
    if let Some(path) = file {
        controller.load_path(path);
    }
    if let Err(e) = controller.run() {
        eprintln!("error: event loop: {e}");
        std::process::exit(1);
    }
}

// ---------------------------------------------------------------------------
// screenshot — headless UI render
// ---------------------------------------------------------------------------

fn screenshot_command(args: &[String]) {
    let mut output: Option<String> = None;
    let mut input: Option<String> = None;
    let mut params = VocalParams::neutral();
    let mut locale = "en".to_string();
    let mut width = 1280u32;
    let mut height = 800u32;
    let mut playhead: Option<f64> = None;

    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        match a.as_str() {
            "--pitch" => {
                i += 1;
                params.pitch_semitones =
                    args.get(i).and_then(|v| v.parse().ok()).unwrap_or_else(|| {
                        eprintln!("error: --pitch needs a number");
                        std::process::exit(2);
                    });
            }
            "--air" => {
                i += 1;
                params.air_percent =
                    args.get(i).and_then(|v| v.parse().ok()).unwrap_or_else(|| {
                        eprintln!("error: --air needs a number");
                        std::process::exit(2);
                    });
            }
            "--tract" => {
                i += 1;
                params.tract_mm = args.get(i).and_then(|v| v.parse().ok()).unwrap_or_else(|| {
                    eprintln!("error: --tract needs a number");
                    std::process::exit(2);
                });
            }
            "--locale" => {
                i += 1;
                locale = args.get(i).cloned().unwrap_or_else(|| {
                    eprintln!("error: --locale needs a value (en|ar)");
                    std::process::exit(2);
                });
            }
            "--width" => {
                i += 1;
                width = args.get(i).and_then(|v| v.parse().ok()).unwrap_or_else(|| {
                    eprintln!("error: --width needs a number");
                    std::process::exit(2);
                });
            }
            "--height" => {
                i += 1;
                height = args.get(i).and_then(|v| v.parse().ok()).unwrap_or_else(|| {
                    eprintln!("error: --height needs a number");
                    std::process::exit(2);
                });
            }
            "--playhead" => {
                i += 1;
                playhead = args.get(i).and_then(|v| v.parse().ok());
            }
            other if output.is_none() && other.ends_with(".png") => {
                output = Some(other.to_string())
            }
            other if input.is_none() => input = Some(other.to_string()),
            other => {
                eprintln!("error: unexpected argument '{other}'");
                std::process::exit(2);
            }
        }
        i += 1;
    }
    let Some(output) = output else {
        eprintln!("usage: micro-vocal-lab screenshot OUT.png [FILE] [options]");
        std::process::exit(2);
    };

    headless::install();
    let app = mvl_app::AppWindow::new().expect("headless component");
    let controller = Controller::new(app);
    if let Some(path) = input {
        let path = std::path::PathBuf::from(path);
        match mvl_io::import(&path) {
            Ok(audio) => {
                let name = path
                    .file_stem()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "audio".into());
                controller.load_audio_sync(audio, name);
            }
            Err(e) => {
                eprintln!("error: import {path:?}: {e}");
                std::process::exit(1);
            }
        }
    }
    if locale == "ar" {
        // go through the controller so the global + translations both flip
        let app = controller.window();
        app.invoke_set_language("ar".into());
    }
    controller.set_params(params);
    if let Some(ph) = playhead {
        let app = controller.window();
        app.set_playhead(ph as f32);
        app.set_show_playhead(true);
    }
    // One honest UI tick: the analysis rack, meters and telemetry show
    // the same data the 30 fps timer would have published.
    controller.refresh();

    let size = slint::PhysicalSize::new(width, height);
    match headless::render_to_png(controller.window(), size, std::path::Path::new(&output)) {
        Ok(()) => println!("wrote {output} ({width}x{height})"),
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}

// ---------------------------------------------------------------------------
// selftest — on-device verification kit (Phase 5, see docs/ONDEVICE.md)
// ---------------------------------------------------------------------------

fn selftest_command(args: &[String]) {
    let mut seconds = 5u32;
    let mut dir: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--seconds" => {
                i += 1;
                match args.get(i).and_then(|v| v.parse::<u32>().ok()) {
                    Some(v) if (1..=60).contains(&v) => seconds = v,
                    _ => {
                        eprintln!("error: --seconds needs a number 1..=60");
                        std::process::exit(2);
                    }
                }
            }
            other if dir.is_none() && !other.starts_with("--") => dir = Some(other.to_string()),
            other => {
                eprintln!("error: unexpected argument '{other}'");
                std::process::exit(2);
            }
        }
        i += 1;
    }
    let out_dir = std::path::PathBuf::from(dir.unwrap_or_else(|| "mvl-selftest".into()));
    if let Err(e) = std::fs::create_dir_all(&out_dir) {
        eprintln!("error: cannot create {}: {e}", out_dir.display());
        std::process::exit(1);
    }
    let code = mvl_app::selftest::run(seconds, &out_dir);
    std::process::exit(code);
}

// ---------------------------------------------------------------------------
// self-check — boot diagnostics
// ---------------------------------------------------------------------------

fn self_check() {
    let params = VocalParams::neutral();
    println!(
        "Micro-Vocal Lab {} — phase 4 (UI)",
        env!("CARGO_PKG_VERSION")
    );
    println!(
        "engine: neutral params ok (pitch {} st, air {} %, tract {} mm)",
        params.pitch_semitones, params.air_percent, params.tract_mm
    );
    println!("engine self-check: {}", engine_self_check());
    println!("audio host: {}", mvl_io::recorder::default_host_name());
    match mvl_io::recorder::default_input_device_name() {
        Ok(Some(name)) => println!("input device: {name}"),
        Ok(None) => println!("input device: none (recording unavailable on this machine)"),
        Err(e) => println!("input device: enumeration failed ({e})"),
    }
    println!(
        "codecs: WAV (hound) + MP3 decode (symphonia 0.5) + MP3 encode (LAME {})",
        mvl_io::mp3::lame_version()
    );
    let slint_version = "1.18.1";
    println!("ui: slint {slint_version} (fluent-dark, embedded IBM Plex, EN/ar + RTL)");
    println!("usage: micro-vocal-lab [run|process|screenshot|selftest|self-check] ...");
}

/// Smoke render used by the boot self-check.
fn engine_self_check() -> String {
    let sig = mvl_core::testsupport::harmonic_stack(220.0, 12, 0.5, 24_000, 48_000);
    match VocalEngine::render(
        &sig,
        48_000,
        VocalParams {
            pitch_semitones: 7.0,
            air_percent: -30,
            tract_mm: 160.0,
        },
        QualityProfile::Preview,
    ) {
        Ok(r) => format!(
            "render ok ({}->{} samples, limiter {})",
            sig.len(),
            r.output.len(),
            if r.limiter_engaged { "engaged" } else { "idle" }
        ),
        Err(e) => format!("render FAILED: {e}"),
    }
}

fn process_command(args: &[String]) {
    let mut input: Option<String> = None;
    let mut output: Option<String> = None;
    let mut params = VocalParams::neutral();
    let mut profile = QualityProfile::Render;
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        match a.as_str() {
            "--pitch" => {
                i += 1;
                match args.get(i).and_then(|v| v.parse::<f32>().ok()) {
                    Some(v) => params.pitch_semitones = v,
                    None => {
                        eprintln!("error: --pitch needs a number (semitones, -12..=12)");
                        std::process::exit(2);
                    }
                }
            }
            "--air" => {
                i += 1;
                match args.get(i).and_then(|v| v.parse::<i32>().ok()) {
                    Some(v) => params.air_percent = v,
                    None => {
                        eprintln!("error: --air needs a number (percent, -100..=100)");
                        std::process::exit(2);
                    }
                }
            }
            "--tract" => {
                i += 1;
                match args.get(i).and_then(|v| v.parse::<f32>().ok()) {
                    Some(v) => params.tract_mm = v,
                    None => {
                        eprintln!("error: --tract needs a number (mm, 100..=260, 170 neutral)");
                        std::process::exit(2);
                    }
                }
            }
            "--profile" => {
                i += 1;
                match args.get(i).map(String::as_str) {
                    Some("preview") => profile = QualityProfile::Preview,
                    Some("render") => profile = QualityProfile::Render,
                    _ => {
                        eprintln!("error: --profile must be preview or render");
                        std::process::exit(2);
                    }
                }
            }
            other if input.is_none() => input = Some(other.to_string()),
            other if output.is_none() => output = Some(other.to_string()),
            other => {
                eprintln!("error: unexpected argument '{other}'");
                std::process::exit(2);
            }
        }
        i += 1;
    }
    let (Some(input), Some(output)) = (input, output) else {
        eprintln!("usage: micro-vocal-lab process <in.wav|in.mp3> <out.wav|out.mp3> [options]");
        std::process::exit(2);
    };

    let audio = match mvl_io::import(std::path::Path::new(&input)) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: import {input}: {e}");
            std::process::exit(1);
        }
    };
    let params = params.sanitized();
    println!(
        "processing {} ({:.2} s, {} ch, {} Hz) with pitch {:+.2} st, air {}%, tract {} mm [{profile:?}]",
        input,
        audio.duration_seconds(),
        audio.channels,
        audio.sample_rate,
        params.pitch_semitones,
        params.air_percent,
        params.tract_mm,
    );
    let started = std::time::Instant::now();
    let processed = match VocalEngine::render_interleaved(
        &audio.data,
        audio.channels,
        audio.sample_rate,
        params,
        profile,
    ) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: engine: {e}");
            std::process::exit(1);
        }
    };
    let elapsed = started.elapsed().as_secs_f64();
    let out_audio = match InterleavedAudio::new(processed, audio.sample_rate, audio.channels) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };
    let out_path = std::path::Path::new(&output);
    let write_result = match out_path.extension().and_then(|e| e.to_str()) {
        Some("mp3") => mvl_io::mp3::export(out_path, &out_audio, &mvl_io::Mp3Settings::default()),
        _ => mvl_io::wav::export(out_path, &out_audio, WavDepth::Float32),
    };
    if let Err(e) = write_result {
        eprintln!("error: export {output}: {e}");
        std::process::exit(1);
    }
    println!(
        "done: {output} (rendered {:.2} s of audio in {elapsed:.2} s = {:.1}x realtime)",
        out_audio.duration_seconds(),
        out_audio.duration_seconds() / elapsed
    );
}
