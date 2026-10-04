//! The controller wires the Slint window to the session model, the
//! preview player and the file dialogs. Every UI action lands here and
//! every state change flows back through window properties — the UI never
//! fabricates values.
//!
//! Threading: all window-property access happens on the Slint event-loop
//! thread. Heavy work (import decode, mipmap build, export render,
//! resampling a finished recording) runs on spawned worker threads that
//! communicate back over a crossbeam channel; the 33 ms UI timer drains
//! that inbox on the event-loop thread. Workers therefore never touch the
//! `Rc` UI state (and the not-`Send` DSP engines never cross threads).

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use mvl_core::params::{MAX_PITCH_SEMITONES, NEUTRAL_TRACT_MM};
use mvl_core::VocalParams;
use mvl_io::recorder::{RecordRequest, Recorder};
use mvl_io::InterleavedAudio;
use slint::ComponentHandle;

use crate::format;
use crate::preview::PreviewPlayer;
use crate::session::{export_session, Session};
use crate::waveform::{render_waveform, ruler_ticks, ViewSpan};

/// How often the UI timer ticks (30 fps refresh + inbox drain).
const UI_TICK_MS: u64 = 33;
/// Recording target (plan §9.2: 192 kHz / mono vocal capture).
const RECORD_REQUEST: RecordRequest = RecordRequest {
    target_rate: 192_000,
    channels: 1,
};

/// Messages from worker threads back to the event loop.
enum UiMessage {
    /// Import or recording build finished.
    SessionReady(Result<Session, String>),
    /// Export finished: Ok(file label, realtime factor) or Err(message).
    ExportDone(Result<(String, String), String>),
    /// Export render progress (0..1).
    ExportProgress(f32),
}

/// Convenience alias for the shared UI state.
type State = Rc<RefCell<Inner>>;

struct Inner {
    session: Option<Session>,
    player: Option<PreviewPlayer>,
    params: VocalParams,
    view: ViewSpan,
    locale: String,
    /// Active capture and its on-disk session path.
    recording: Option<(Recorder, PathBuf)>,
    /// guard against concurrent import/export workers
    busy: Arc<AtomicBool>,
    /// inbox from worker threads (drained by the UI timer).
    inbox: crossbeam_channel::Receiver<UiMessage>,
    /// sender half handed to workers (cloned per spawn).
    inbox_tx: crossbeam_channel::Sender<UiMessage>,
    /// Device picker (BUG 1/3): `None` = automatic (fallback chain);
    /// `Some(device id)` = the user's explicit choice.
    selected_input: Option<String>,
    selected_output: Option<String>,
    /// Cached inventory behind the picker (index → device mapping).
    input_devices: Vec<mvl_io::DeviceInfo>,
    output_devices: Vec<mvl_io::DeviceInfo>,
}

impl Inner {
    fn duration(&self) -> f64 {
        self.session.as_ref().map_or(1.0, |s| s.duration_secs)
    }

    fn preview_rate(&self) -> u32 {
        self.session
            .as_ref()
            .map_or(48_000, |s| s.preview.sample_rate)
    }
}

/// Owns the window handle, shared state and the UI timer. The timer is
/// held (never read) so it stays alive for as long as the controller.
pub struct Controller {
    app: crate::AppWindow,
    state: State,
    #[allow(dead_code)]
    timer: slint::Timer,
}

impl Controller {
    /// Wire every callback and start the UI timer.
    #[must_use]
    pub fn new(app: crate::AppWindow) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded::<UiMessage>();
        let state: State = Rc::new(RefCell::new(Inner {
            session: None,
            player: None,
            params: VocalParams::neutral(),
            view: ViewSpan::full(1.0),
            locale: "en".into(),
            recording: None,
            busy: Arc::new(AtomicBool::new(false)),
            inbox: rx,
            inbox_tx: tx,
            selected_input: None,
            selected_output: None,
            input_devices: Vec::new(),
            output_devices: Vec::new(),
        }));

        macro_rules! wire {
            ($cb:ident, $fn:ident) => {{
                let weak = app.as_weak();
                let st = Rc::clone(&state);
                app.$cb(move || {
                    if let Some(app) = weak.upgrade() {
                        $fn(&app, &st);
                    }
                });
            }};
        }

        wire!(on_import_clicked, import_clicked);
        wire!(on_export_clicked, export_clicked);
        wire!(on_new_recording_clicked, toggle_recording);
        wire!(on_play_pause_clicked, play_pause);
        wire!(on_stop_clicked, stop_playback);
        wire!(on_rewind_clicked, rewind);
        wire!(on_zoom_fit_clicked, zoom_fit);
        wire!(on_canvas_resized, refresh_waveform);
        wire!(on_reset_pitch, reset_pitch);
        wire!(on_reset_air, reset_air);
        wire!(on_reset_tract, reset_tract);
        wire!(on_devices_clicked, devices_clicked);
        wire!(on_close_devices_clicked, close_devices_clicked);

        {
            let weak = app.as_weak();
            let st = Rc::clone(&state);
            app.on_set_input_device(move |label| {
                if let Some(app) = weak.upgrade() {
                    set_input_device(&app, &st, label.as_str());
                }
            });
        }
        {
            let weak = app.as_weak();
            let st = Rc::clone(&state);
            app.on_set_output_device(move |label| {
                if let Some(app) = weak.upgrade() {
                    set_output_device(&app, &st, label.as_str());
                }
            });
        }

        {
            let weak = app.as_weak();
            let st = Rc::clone(&state);
            app.on_seek_seconds(move |t| {
                if let Some(app) = weak.upgrade() {
                    seek_to(&app, &st, f64::from(t));
                }
            });
        }
        {
            let weak = app.as_weak();
            let st = Rc::clone(&state);
            app.on_zoom_at(move |f, dir| {
                if let Some(app) = weak.upgrade() {
                    zoom(&app, &st, f64::from(f), dir);
                }
            });
        }
        {
            let weak = app.as_weak();
            let st = Rc::clone(&state);
            app.on_select_seconds(move |a, b| {
                if let Some(app) = weak.upgrade() {
                    set_selection(&app, &st, f64::from(a), f64::from(b));
                }
            });
        }
        {
            let weak = app.as_weak();
            let st = Rc::clone(&state);
            app.on_set_pitch(move |v| {
                if let Some(app) = weak.upgrade() {
                    set_pitch(&app, &st, f64::from(v) as f32);
                }
            });
        }
        {
            let weak = app.as_weak();
            let st = Rc::clone(&state);
            app.on_set_air(move |v| {
                if let Some(app) = weak.upgrade() {
                    set_air(&app, &st, (f64::from(v)).round() as i32);
                }
            });
        }
        {
            let weak = app.as_weak();
            let st = Rc::clone(&state);
            app.on_set_tract(move |v| {
                if let Some(app) = weak.upgrade() {
                    set_tract(&app, &st, f64::from(v) as f32);
                }
            });
        }
        {
            let weak = app.as_weak();
            let st = Rc::clone(&state);
            app.on_commit_pitch_text(move |t| {
                if let Some(app) = weak.upgrade() {
                    commit_pitch(&app, &st, &t);
                }
            });
        }
        {
            let weak = app.as_weak();
            let st = Rc::clone(&state);
            app.on_commit_air_text(move |t| {
                if let Some(app) = weak.upgrade() {
                    commit_air(&app, &st, &t);
                }
            });
        }
        {
            let weak = app.as_weak();
            let st = Rc::clone(&state);
            app.on_commit_tract_text(move |t| {
                if let Some(app) = weak.upgrade() {
                    commit_tract(&app, &st, &t);
                }
            });
        }
        {
            let weak = app.as_weak();
            let st = Rc::clone(&state);
            app.on_set_language(move |l| {
                if let Some(app) = weak.upgrade() {
                    set_language(&app, &st, &l);
                }
            });
        }

        // periodic refresh + inbox drain
        let timer = slint::Timer::default();
        {
            let weak = app.as_weak();
            let st = Rc::clone(&state);
            timer.start(
                slint::TimerMode::Repeated,
                std::time::Duration::from_millis(UI_TICK_MS),
                move || {
                    if let Some(app) = weak.upgrade() {
                        ui_tick(&app, &st);
                    }
                },
            );
        }

        let controller = Self { app, state, timer };
        push_all_params(&controller.app, &controller.state);
        set_status(&controller.app, "no-audio", "", "");
        controller
    }

    /// Run the event loop (desktop).
    ///
    /// # Errors
    /// Slint platform errors (no display, renderer failure).
    pub fn run(&self) -> Result<(), slint::PlatformError> {
        self.app.run()
    }

    /// Load an audio file path (CLI path; decode runs on a worker thread
    /// exactly like the dialog path).
    pub fn load_path(&self, path: PathBuf) {
        import_async(&self.app, &self.state, path);
    }

    /// Load decoded audio synchronously (screenshot/tests path).
    pub fn load_audio_sync(&self, audio: InterleavedAudio, name: String) {
        let session = Session::from_imported(audio, name);
        install_session(&self.app, &self.state, session, None);
    }

    /// Override parameters programmatically (screenshot/tests path).
    pub fn set_params(&self, params: VocalParams) {
        {
            let mut inner = self.state.borrow_mut();
            inner.params = params.sanitized();
            if let Some(player) = &inner.player {
                let _ = player.set_params(inner.params);
            }
        }
        push_all_params(&self.app, &self.state);
    }

    /// Current parameters.
    #[must_use]
    pub fn params(&self) -> VocalParams {
        self.state.borrow().params
    }

    /// Window handle access (tests, screenshot renderer).
    #[must_use]
    pub fn window(&self) -> &crate::AppWindow {
        &self.app
    }

    /// Worker-thread inbox sender (cloned out of the shared state).
    fn inbox_tx(state: &State) -> crossbeam_channel::Sender<UiMessage> {
        state.borrow().inbox_tx.clone()
    }
}

// ---------------------------------------------------------------------------
// actions (free functions over (app, state))
// ---------------------------------------------------------------------------

fn import_clicked(app: &crate::AppWindow, state: &State) {
    if state.borrow().busy.load(Ordering::Acquire) || state.borrow().recording.is_some() {
        return;
    }
    let Some(path) = rfd::FileDialog::new()
        .add_filter("Audio (WAV, MP3)", &["wav", "wave", "mp3"])
        .set_title("Import audio")
        .pick_file()
    else {
        return;
    };
    import_async(app, state, path);
}

fn import_async(app: &crate::AppWindow, state: &State, path: PathBuf) {
    let busy = state.borrow().busy.clone();
    if busy.swap(true, Ordering::AcqRel) {
        return;
    }
    app.set_busy(true);
    let label = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    set_status(app, "importing", &label, "");
    let tx = Controller::inbox_tx(state);
    std::thread::spawn(move || {
        // BUG 2 fix: a panic inside hound/symphonia used to unwind this
        // worker silently — SessionReady never arrived, the busy flag
        // stayed up, and the UI froze forever (reported as "the program
        // closes"). catch_unwind converts any library panic into a typed
        // error the status bar can show.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            mvl_io::import(&path)
                .map_err(|e| e.to_string())
                .map(|audio| {
                    let name = path
                        .file_stem()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| "audio".into());
                    Session::from_imported(audio, name)
                })
        }))
        .unwrap_or_else(|payload| Err(panic_message(&payload, "import")));
        let _ = tx.send(UiMessage::SessionReady(result));
    });
}

/// Render a caught panic payload into a user-presentable string.
fn panic_message(payload: &Box<dyn std::any::Any + Send>, what: &str) -> String {
    let detail = payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("unknown internal error");
    format!("{what} failed unexpectedly: {detail}. This is a bug — the file may be damaged; other files will keep working.")
}

fn export_clicked(app: &crate::AppWindow, state: &State) {
    if state.borrow().busy.load(Ordering::Acquire) {
        return;
    }
    let (default_name, session) = {
        let inner = state.borrow();
        let Some(session) = &inner.session else {
            return;
        };
        (
            format!("processed-{}.wav", session.display_name),
            session.clone(),
        )
    };
    let Some(path) = rfd::FileDialog::new()
        .add_filter("WAV (32-bit float)", &["wav"])
        .add_filter("MP3", &["mp3"])
        .set_title("Export processed audio")
        .set_file_name(&default_name)
        .save_file()
    else {
        return;
    };

    let busy = state.borrow().busy.clone();
    if busy.swap(true, Ordering::AcqRel) {
        return;
    }
    app.set_busy(true);
    app.set_export_progress(0);
    set_status(app, "rendering", "", "");
    let tx = Controller::inbox_tx(state);
    let params = state.borrow().params;
    std::thread::spawn(move || {
        // BUG 4: render/export panics must surface as typed errors, not
        // freeze the busy flag.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let progress_tx = tx.clone();
            export_session(&session, params, &path, &|p| {
                let _ = progress_tx.send(UiMessage::ExportProgress(p));
            })
        }))
        .unwrap_or_else(|payload| {
            Err(mvl_io::Error::InvalidState(panic_message(
                &payload, "export",
            )))
        });
        let msg = match outcome {
            Ok(o) => Ok((
                path.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default(),
                format!("{:.1}", o.realtime_factor()),
            )),
            Err(e) => Err(e.to_string()),
        };
        let _ = tx.send(UiMessage::ExportDone(msg));
    });
}

// ---- device picker (BUG 1/3) --------------------------------------------

/// Open the devices dialog: refresh the inventory (devices may have been
/// plugged/unplugged since launch) and publish real names to the UI.
fn devices_clicked(app: &crate::AppWindow, state: &State) {
    let inv = mvl_io::devices::list();
    let (in_idx, out_idx) = {
        let mut inner = state.borrow_mut();
        inner.input_devices = inv.inputs.clone();
        inner.output_devices = inv.outputs.clone();
        (
            match &inner.selected_input {
                None => 0,
                Some(id) => inner
                    .input_devices
                    .iter()
                    .position(|d| d.id() == *id)
                    .map_or(0, |p| p as i32 + 1),
            },
            match &inner.selected_output {
                None => 0,
                Some(id) => inner
                    .output_devices
                    .iter()
                    .position(|d| d.id() == *id)
                    .map_or(0, |p| p as i32 + 1),
            },
        )
    };

    let auto_label = AUTO_DEVICE_LABEL;
    let mut in_names = vec![auto_label.to_string()];
    in_names.extend(inv.inputs.iter().map(|d| d.label()));
    let mut out_names = vec![auto_label.to_string()];
    out_names.extend(inv.outputs.iter().map(|d| d.label()));

    let mut summary = format!(
        "{} input(s) · {} output(s) · host: {}",
        inv.inputs.len(),
        inv.outputs.len(),
        mvl_io::recorder::default_host_name(),
    );
    if !inv.hosts_failed.is_empty() {
        let fails: Vec<String> = inv.hosts_failed.iter().map(|(h, _)| h.clone()).collect();
        summary.push_str(&format!(" · failed hosts: {}", fails.join(", ")));
    }
    if inv.inputs.is_empty() {
        summary.push_str("\nNo capture device visible — check OS microphone permissions.");
    }

    let in_model: slint::VecModel<slint::SharedString> = slint::VecModel::from(
        in_names
            .into_iter()
            .map(slint::SharedString::from)
            .collect::<Vec<_>>(),
    );
    let out_model: slint::VecModel<slint::SharedString> = slint::VecModel::from(
        out_names
            .into_iter()
            .map(slint::SharedString::from)
            .collect::<Vec<_>>(),
    );
    app.set_input_device_names(slint::ModelRc::new(in_model));
    app.set_output_device_names(slint::ModelRc::new(out_model));
    app.set_input_device_index(in_idx);
    app.set_output_device_index(out_idx);
    app.set_device_summary_text(summary.as_str().into());
    app.set_show_devices(true);
}

fn close_devices_clicked(app: &crate::AppWindow, _state: &State) {
    app.set_show_devices(false);
}

/// Label of the automatic entry (must match what `devices_clicked`
/// publishes as element 0).
const AUTO_DEVICE_LABEL: &str = "Automatic (all devices)";

/// Resolve a ComboBox label back to a device id.
fn device_id_for_label(devices: &[mvl_io::DeviceInfo], label: &str) -> Option<String> {
    if label == AUTO_DEVICE_LABEL {
        return None;
    }
    devices.iter().find(|d| d.label() == label).map(|d| d.id())
}

/// ComboBox selection: "Automatic…" = fallback chain; a device label
/// pins recording to that device.
fn set_input_device(app: &crate::AppWindow, state: &State, label: &str) {
    let id = device_id_for_label(&state.borrow().input_devices, label);
    let index = match &id {
        None => 0,
        Some(id) => state
            .borrow()
            .input_devices
            .iter()
            .position(|d| d.id() == *id)
            .map_or(0, |p| p as i32 + 1),
    };
    state.borrow_mut().selected_input = id.clone();
    app.set_input_device_index(index);
    // The next recording uses the new device; the status line confirms it.
    match &id {
        Some(id) => set_status(app, "audio-loaded", &format!("input: {id}"), ""),
        None => set_status(app, "ready", "", ""),
    }
}

fn set_output_device(app: &crate::AppWindow, state: &State, label: &str) {
    let id = device_id_for_label(&state.borrow().output_devices, label);
    let index = match &id {
        None => 0,
        Some(id) => state
            .borrow()
            .output_devices
            .iter()
            .position(|d| d.id() == *id)
            .map_or(0, |p| p as i32 + 1),
    };
    {
        let mut inner = state.borrow_mut();
        inner.selected_output = id;
        // rebuild the player on the new device at next play
        inner.player = None;
    }
    app.set_output_device_index(index);
    app.set_playing(false);
    app.set_paused(false);
}

/// Open the user-selected input device, if any (recording path).
fn open_selected_input(state: &State) -> Option<Result<cpal::Device, mvl_io::Error>> {
    let id = state.borrow().selected_input.clone()?;
    Some(mvl_io::devices::open_by_id(&id))
}

fn toggle_recording(app: &crate::AppWindow, state: &State) {
    let recorder = state.borrow_mut().recording.take();
    if let Some((recorder, temp_path)) = recorder {
        // Stopping: `Recorder::stop` joins the writer and finalizes the
        // WAV quickly on this thread; the slow part (192 kHz → 48 kHz
        // preview resample + mipmap) goes to a worker.
        let busy = state.borrow().busy.clone();
        busy.store(true, Ordering::Release);
        app.set_busy(true);
        app.set_recording(false);
        set_status(app, "importing", "recording", "");
        let result = recorder.stop().map_err(|e| e.to_string());
        let tx = Controller::inbox_tx(state);
        match result {
            Ok(stats) if stats.frames == 0 => {
                // Tapped stop before any audio arrived — an empty session
                // would be useless (and used to render a zero-frame
                // waveform, the BUG 2 panic path).
                busy.store(false, Ordering::Release);
                app.set_busy(false);
                set_status(
                    app,
                    "import-error",
                    "Recording was empty (stopped before any audio arrived)",
                    "",
                );
                let _ = std::fs::remove_file(&temp_path);
            }
            Ok(_stats) => {
                std::thread::spawn(move || {
                    // BUG 4: a panic while building the preview (resample/
                    // mipmap) must not strand the busy flag.
                    let built = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        Session::from_recording(&temp_path).map_err(|e| e.to_string())
                    }))
                    .unwrap_or_else(|payload| Err(panic_message(&payload, "recording load")));
                    let _ = tx.send(UiMessage::SessionReady(built));
                });
            }
            Err(e) => {
                busy.store(false, Ordering::Release);
                app.set_busy(false);
                set_status(app, "import-error", &e, "");
            }
        }
    } else {
        // starting a new recording into a temp session WAV — on the
        // user-selected device when one is pinned, else the automatic
        // every-device fallback chain (BUG 1 fix).
        let temp = recording_path();
        let started = match open_selected_input(state) {
            Some(Ok(dev)) => Recorder::start_on(&dev, &temp, RECORD_REQUEST),
            Some(Err(e)) => {
                // the pinned device vanished (unplugged): fall back to
                // automatic rather than failing, and say so
                eprintln!("mvl-io: selected input gone, falling back: {e}");
                Recorder::start(&temp, RECORD_REQUEST)
            }
            None => Recorder::start(&temp, RECORD_REQUEST),
        };
        match started {
            Ok(recorder) => {
                state.borrow_mut().recording = Some((recorder, temp));
                app.set_recording(true);
                set_status(app, "recording", "00:00", "192 kHz");
            }
            Err(e) => {
                let msg = e.to_string();
                let key = if msg.to_lowercase().contains("input") {
                    "no-input-device"
                } else {
                    "import-error"
                };
                set_status(app, key, &msg, "");
            }
        }
    }
}

fn play_pause(app: &crate::AppWindow, state: &State) {
    let playing = state
        .borrow()
        .player
        .as_ref()
        .map(|p| p.state() == mvl_io::TransportState::Playing)
        .unwrap_or(false);
    if playing {
        if let Some(p) = &state.borrow().player {
            let _ = p.pause();
        }
        app.set_playing(false);
        app.set_paused(true);
        return;
    }

    let mut inner = state.borrow_mut();
    if inner.player.is_none() {
        let preview = inner.session.as_ref().map(|s| Arc::clone(&s.preview));
        let selected_output = inner.selected_output.clone();
        let built = match (&preview, &selected_output) {
            (Some(src), Some(id)) => {
                match mvl_io::devices::open_by_id(id) {
                    Ok(dev) => PreviewPlayer::on_device(&dev, src).or_else(|e| {
                        // pinned device failed: fall back to automatic
                        eprintln!("mvl-app: selected output failed, falling back: {e}");
                        PreviewPlayer::new(src)
                    }),
                    Err(e) => {
                        eprintln!("mvl-app: selected output gone, falling back: {e}");
                        PreviewPlayer::new(src)
                    }
                }
            }
            (Some(src), None) => PreviewPlayer::new(src),
            (None, _) => return,
        };
        match built {
            Ok(player) => {
                let _ = player.set_params(inner.params);
                let latency = format!("preview {:.1} ms", player.latency_ms());
                inner.player = Some(player);
                drop(inner);
                app.set_preview_latency_text(latency.as_str().into());
            }
            Err(e) => {
                drop(inner);
                let msg = e.to_string();
                let key = if msg.to_lowercase().contains("output") {
                    "no-output-device"
                } else {
                    "export-error"
                };
                set_status(app, key, &msg, "");
                return;
            }
        }
    } else {
        let params = inner.params;
        if let Some(p) = &inner.player {
            let _ = p.set_params(params);
        }
        drop(inner);
    }
    if let Some(p) = &state.borrow().player {
        let _ = p.play();
    }
    app.set_playing(true);
    app.set_paused(false);
}

fn stop_playback(app: &crate::AppWindow, state: &State) {
    let mut inner = state.borrow_mut();
    if let Some(p) = &inner.player {
        let _ = p.stop();
    }
    inner.player = None; // fresh engines on next play
    drop(inner);
    app.set_playing(false);
    app.set_paused(false);
    app.set_show_playhead(false);
    app.set_position_text(format::timecode(0.0).into());
    app.set_playhead(0.0);
}

fn rewind(app: &crate::AppWindow, state: &State) {
    seek_to(app, state, 0.0);
}

fn seek_to(app: &crate::AppWindow, state: &State, seconds: f64) {
    let t = seconds.clamp(0.0, state.borrow().duration());
    if let Some(p) = &state.borrow().player {
        let _ = p.seek(t);
    }
    app.set_playhead(t as f32);
    app.set_position_text(format::timecode(t).into());
    app.set_show_playhead(true);
}

fn zoom(app: &crate::AppWindow, state: &State, anchor: f64, direction: i32) {
    let mut inner = state.borrow_mut();
    let d = inner.duration();
    let rate = inner.preview_rate();
    let factor = if direction > 0 { 0.6 } else { 1.0 / 0.6 };
    inner.view = inner.view.zoomed(anchor, factor, d, rate);
    let view = inner.view;
    drop(inner);
    push_view(app, state, view);
}

fn zoom_fit(app: &crate::AppWindow, state: &State) {
    let mut inner = state.borrow_mut();
    inner.view = ViewSpan::full(inner.duration());
    let view = inner.view;
    drop(inner);
    push_view(app, state, view);
}

fn set_selection(app: &crate::AppWindow, state: &State, a: f64, b: f64) {
    let d = state.borrow().duration();
    let (a, b) = (a.clamp(0.0, d), b.clamp(0.0, d));
    app.set_sel_start(a as f32);
    app.set_sel_end(b as f32);
    app.set_has_selection(true);
}

fn refresh_waveform(app: &crate::AppWindow, state: &State) {
    let inner = state.borrow();
    let Some(session) = &inner.session else {
        return;
    };
    let view = inner.view;
    let rate = f64::from(session.preview.sample_rate);
    let w = app.get_canvas_width().max(1.0) as usize;
    let h = app.get_canvas_height().max(1.0) as usize;
    let f0 = (view.start * rate).round() as usize;
    let f1 = ((view.end * rate).round() as usize).max(f0 + 1);
    let image = render_waveform(
        &session.mipmap,
        session.preview.channels as usize,
        f0,
        f1,
        w,
        h,
        session.preview.sample_rate,
    );
    let ticks: Vec<crate::RulerTick> = ruler_ticks(view.start, view.end, w as f64, 70.0)
        .into_iter()
        .map(|t| crate::RulerTick {
            fraction: t.fraction,
            label: t.label.into(),
            major: t.major,
        })
        .collect();
    let sample_level = view.sample_level(session.preview.sample_rate);
    drop(inner);
    app.set_wave(image);
    app.set_ticks(slint::ModelRc::new(slint::VecModel::from(ticks)));
    app.set_sample_level(sample_level);
}

fn push_view(app: &crate::AppWindow, state: &State, view: ViewSpan) {
    app.set_view_start(view.start as f32);
    app.set_view_end(view.end as f32);
    refresh_waveform(app, state);
}

// ---- parameters -----------------------------------------------------------

fn set_pitch(app: &crate::AppWindow, state: &State, v: f32) {
    {
        let mut inner = state.borrow_mut();
        inner.params.pitch_semitones = v;
        let params = inner.params;
        if let Some(p) = &inner.player {
            let _ = p.set_params(params);
        }
    }
    push_all_params(app, state);
}

fn set_air(app: &crate::AppWindow, state: &State, v: i32) {
    {
        let mut inner = state.borrow_mut();
        inner.params.air_percent = v;
        let params = inner.params;
        if let Some(p) = &inner.player {
            let _ = p.set_params(params);
        }
    }
    push_all_params(app, state);
}

fn set_tract(app: &crate::AppWindow, state: &State, v: f32) {
    {
        let mut inner = state.borrow_mut();
        inner.params.tract_mm = v;
        let params = inner.params;
        if let Some(p) = &inner.player {
            let _ = p.set_params(params);
        }
    }
    push_all_params(app, state);
}

fn reset_pitch(app: &crate::AppWindow, state: &State) {
    set_pitch(app, state, 0.0);
}

fn reset_air(app: &crate::AppWindow, state: &State) {
    set_air(app, state, 0);
}

fn reset_tract(app: &crate::AppWindow, state: &State) {
    set_tract(app, state, NEUTRAL_TRACT_MM);
}

fn commit_pitch(app: &crate::AppWindow, state: &State, text: &str) {
    match format::parse_f32(text) {
        Some(v) => set_pitch(
            app,
            state,
            v.clamp(-MAX_PITCH_SEMITONES, MAX_PITCH_SEMITONES),
        ),
        None => push_all_params(app, state), // restore canonical text
    }
}

fn commit_air(app: &crate::AppWindow, state: &State, text: &str) {
    match format::air_percent_from_db_text(text) {
        Some(v) => set_air(app, state, v),
        None => push_all_params(app, state),
    }
}

fn commit_tract(app: &crate::AppWindow, state: &State, text: &str) {
    match format::parse_f32(text) {
        Some(v) => set_tract(app, state, v.clamp(100.0, 260.0)),
        None => push_all_params(app, state),
    }
}

fn push_all_params(app: &crate::AppWindow, state: &State) {
    let p = state.borrow().params.sanitized();
    app.set_pitch_semitones(p.pitch_semitones);
    app.set_air_percent(p.air_percent);
    app.set_tract_mm(p.tract_mm);
    app.set_pitch_value_text(format::pitch_field_text(p.pitch_semitones).into());
    app.set_pitch_detail_text(format::note_from_semitones(p.pitch_semitones).into());
    app.set_air_value_text(format::air_db_text(p.air_percent).into());
    app.set_tract_value_text(format::tract_field_text(p.tract_mm).into());
    app.set_tract_detail_text(format::tract_interval_st(p.tract_mm).into());
    // keep the editable fields in sync with canonical values
    app.invoke_set_pitch_field(format::pitch_field_text(p.pitch_semitones).into());
    app.invoke_set_air_field(format::air_field_text(p.air_percent).into());
    app.invoke_set_tract_field(format::tract_field_text(p.tract_mm).into());
}

// ---- language -------------------------------------------------------------

fn set_language(app: &crate::AppWindow, state: &State, locale: &str) {
    state.borrow_mut().locale = locale.to_string();
    app.global::<crate::Translations>()
        .set_locale(locale.into());
    let result = slint::select_bundled_translation(if locale == "ar" { "ar" } else { "" });
    if let Err(e) = result {
        eprintln!("mvl-app: translation select failed for {locale}: {e:?}");
    }
}

// ---- session install + status ----------------------------------------------

fn install_session(
    app: &crate::AppWindow,
    state: &State,
    session: Session,
    params: Option<VocalParams>,
) {
    let duration = session.duration_secs;
    let name = session.display_name.clone();
    let rate = session.sample_rate;
    let depth = session.bit_depth_text.clone();
    let channels = session.channels;
    {
        let mut inner = state.borrow_mut();
        if let Some(p) = params {
            inner.params = p;
        }
        inner.player = None; // rebuild lazily for the new material
        inner.view = ViewSpan::full(duration);
        inner.session = Some(session);
    }
    app.set_has_audio(true);
    app.set_file_name(name.as_str().into());
    app.set_duration_text(format::timecode(duration).into());
    app.set_position_text(format::timecode(0.0).into());
    app.set_rate_text(format!("{rate} Hz · {channels} ch").into());
    app.set_depth_text(depth.as_str().into());
    app.set_playing(false);
    app.set_paused(false);
    app.set_show_playhead(true);
    app.set_has_selection(false);
    app.set_air_meter_percent(0);
    push_view(app, state, ViewSpan::full(duration));
    push_all_params(app, state);
    let label = format!(
        "{name} · {rate} Hz · {channels} ch · {}",
        format::timecode(duration)
    );
    set_status(app, "audio-loaded", &label, "");
}

fn set_status(app: &crate::AppWindow, key: &str, a1: &str, a2: &str) {
    app.set_status_key(key.into());
    app.set_status_a1(a1.into());
    app.set_status_a2(a2.into());
}

// ---- timer ------------------------------------------------------------------

fn ui_tick(app: &crate::AppWindow, state: &State) {
    // 1) drain the worker inbox
    while let Ok(msg) = state.borrow().inbox.try_recv() {
        match msg {
            UiMessage::SessionReady(result) => {
                let busy = state.borrow().busy.clone();
                busy.store(false, Ordering::Release);
                app.set_busy(false);
                match result {
                    Ok(session) => install_session(app, state, session, None),
                    Err(e) => set_status(app, "import-error", &e, ""),
                }
            }
            UiMessage::ExportProgress(p) => {
                app.set_export_progress((p * 100.0).round() as i32);
            }
            UiMessage::ExportDone(result) => {
                let busy = state.borrow().busy.clone();
                busy.store(false, Ordering::Release);
                app.set_busy(false);
                app.set_export_progress(0);
                match result {
                    Ok((label, speed)) => set_status(app, "exported", &label, &speed),
                    Err(e) => set_status(app, "export-error", &e, ""),
                }
            }
        }
    }

    // 2) live transport + telemetry
    let inner = state.borrow();
    let playing = inner
        .player
        .as_ref()
        .map(|p| p.state() == mvl_io::TransportState::Playing)
        .unwrap_or(false);
    let paused = inner
        .player
        .as_ref()
        .map(|p| p.state() == mvl_io::TransportState::Paused)
        .unwrap_or(false);
    app.set_playing(playing);
    app.set_paused(paused);

    if let Some(player) = &inner.player {
        let pos = player.position_seconds().min(inner.duration());
        app.set_playhead(pos as f32);
        app.set_position_text(format::timecode(pos).into());
        app.set_show_playhead(true);
        // live air telemetry (applied duck gain)
        let applied = player.applied_air_db();
        let meter = if applied < -0.05 {
            (applied / 0.4).round().clamp(-100.0, 0.0) as i32
        } else {
            0
        };
        app.set_air_meter_percent(meter);
        let air_detail = if applied < -0.05 {
            format!("applied {applied:.1} dB")
        } else if inner.params.air_percent > 0 {
            format!("air injection +{} %", inner.params.air_percent)
        } else {
            "bypass (neutral)".to_string()
        };
        app.set_air_detail_text(air_detail.into());
    }

    // 3) recording elapsed
    if let Some((rec, _path)) = &inner.recording {
        let elapsed = format::elapsed_mmss(rec.elapsed().as_secs_f64());
        app.set_recording_elapsed(elapsed.as_str().into());
        app.set_status_a1(elapsed.into());
    }
}

fn recording_path() -> PathBuf {
    let mut p = std::env::temp_dir();
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    p.push(format!("mvl-session-{stamp}.wav"));
    p
}
