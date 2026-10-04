//! The session model (plan §9.2): one loaded piece of audio with its
//! artifacts.
//!
//! * the **preview copy** (≤ 48 kHz) lives in RAM and feeds the waveform
//!   and the real-time preview player;
//! * the **export source** stays wherever it is cheapest: imported files
//!   are already in RAM; 192 kHz recordings stay on disk and are streamed
//!   chunk-by-chunk through the render-profile engines at export time, so
//!   RAM stays independent of take length.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use mvl_core::{QualityProfile, VocalEngine, VocalParams};
use mvl_io::{InterleavedAudio, WavDepth};

use crate::waveform::PeakMipmap;

/// Sample rate of the real-time preview path (plan §6.6).
pub const PREVIEW_RATE: u32 = 48_000;

/// Where export renders read from.
#[derive(Clone)]
pub enum ExportSource {
    /// Imported file, already decoded in RAM (renders via the exact
    /// offline path the Phase 3 fixtures used).
    InMemory(Arc<InterleavedAudio>),
    /// Recorded full-rate session on disk (streamed render, §9.2).
    OnDisk(PathBuf),
}

/// A loaded session.
#[derive(Clone)]
pub struct Session {
    /// ≤ 48 kHz preview copy (waveform + preview player source).
    pub preview: Arc<InterleavedAudio>,
    /// Peak mipmap over the preview copy.
    pub mipmap: Arc<PeakMipmap>,
    /// Human-readable name (file stem or "Recording").
    pub display_name: String,
    /// Duration of the original material in seconds.
    pub duration_secs: f64,
    /// Original sample rate (display + render rate).
    pub sample_rate: u32,
    /// Channel count of the original material.
    pub channels: u16,
    /// Original bit depth, for the status bar.
    pub bit_depth_text: String,
    /// What export reads from.
    pub export_source: ExportSource,
}

impl Session {
    /// Build a session from an imported (decoded) file.
    #[must_use]
    pub fn from_imported(audio: InterleavedAudio, name: String) -> Self {
        let sample_rate = audio.sample_rate;
        let channels = audio.channels;
        let duration = audio.duration_seconds();
        let export = Arc::new(audio);
        let preview = preview_copy(&export);
        let mipmap = Arc::new(PeakMipmap::build(&preview.data, preview.channels as usize));
        Self {
            display_name: name,
            duration_secs: duration,
            sample_rate,
            channels,
            bit_depth_text: "32-bit float".into(),
            mipmap,
            preview: Arc::new(preview),
            export_source: ExportSource::InMemory(export),
        }
    }

    /// Build a session from a finished recording on disk (plan §9.2):
    /// the full-rate WAV stays on disk; a 48 kHz preview copy is streamed
    /// out of it via the disk-based resampler, so the full-rate audio
    /// never enters RAM.
    ///
    /// # Errors
    /// Propagates `mvl_io` resample/import errors.
    pub fn from_recording(path: &Path) -> mvl_io::Result<Self> {
        let preview_path = path.with_extension("preview48k.wav");
        mvl_io::resample::resample_file(path, &preview_path, PREVIEW_RATE)?;
        let preview = mvl_io::wav::import(&preview_path)?;
        let (rate, frames) = hound::WavReader::open(path)
            .map(|r| (r.spec().sample_rate, r.duration()))
            .map_err(|e| mvl_io::Error::Wav(format!("{}: {e}", path.display())))?;
        let duration = frames as f64 / f64::from(rate);
        let mipmap = Arc::new(PeakMipmap::build(&preview.data, preview.channels as usize));
        Ok(Self {
            duration_secs: duration,
            sample_rate: rate,
            channels: preview.channels,
            bit_depth_text: "32-bit float".into(),
            display_name: "Recording".into(),
            preview: Arc::new(preview),
            mipmap,
            export_source: ExportSource::OnDisk(path.to_path_buf()),
        })
    }
}

fn preview_copy(audio: &InterleavedAudio) -> InterleavedAudio {
    if audio.sample_rate <= PREVIEW_RATE {
        audio.clone()
    } else {
        mvl_io::resample::resample(audio, PREVIEW_RATE).unwrap_or_else(|_| audio.clone())
        // fall back: show at native rate
    }
}

/// Chosen export container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Wav,
    Mp3,
}

impl ExportFormat {
    /// Detect from a file extension.
    #[must_use]
    pub fn from_path(path: &Path) -> Self {
        match path.extension().and_then(|e| e.to_str()) {
            Some(e) if e.eq_ignore_ascii_case("mp3") => Self::Mp3,
            _ => Self::Wav,
        }
    }
}

/// What an export produced (status bar evidence).
#[derive(Debug, Clone)]
pub struct ExportOutcome {
    pub audio_seconds: f64,
    pub render_seconds: f64,
    pub guard_engaged: bool,
}

impl ExportOutcome {
    /// Render speed as a multiple of realtime.
    #[must_use]
    pub fn realtime_factor(&self) -> f64 {
        if self.render_seconds > 0.0 {
            self.audio_seconds / self.render_seconds
        } else {
            f64::INFINITY
        }
    }
}

/// Render the session with render-profile quality and write it out.
///
/// In-memory sources use the exact offline path of the Phase 3 fixtures
/// (`VocalEngine::render_interleaved`); on-disk 192 kHz sessions are
/// streamed through the engines in chunks so RAM stays flat (§9.2). MP3
/// from an on-disk session decodes to RAM first (gap: stream-decode —
/// noted in the phase report).
///
/// `progress` receives 0.0..1.0.
///
/// # Errors
/// `mvl_io` errors from import/render/export; engine errors wrapped as
/// `Error::InvalidAudio`.
pub fn export_session(
    session: &Session,
    params: VocalParams,
    path: &Path,
    progress: &dyn Fn(f32),
) -> mvl_io::Result<ExportOutcome> {
    progress(0.0);
    let started = Instant::now();
    let format = ExportFormat::from_path(path);
    let outcome = match (&session.export_source, format) {
        (ExportSource::InMemory(audio), ExportFormat::Wav) => {
            let rendered = VocalEngine::render_interleaved(
                &audio.data,
                audio.channels,
                audio.sample_rate,
                params,
                QualityProfile::Render,
            )
            .map_err(|e| mvl_io::Error::InvalidAudio(format!("engine: {e}")))?;
            progress(0.9);
            let out = InterleavedAudio::new(rendered, audio.sample_rate, audio.channels)?;
            mvl_io::wav::export(path, &out, WavDepth::Float32)?;
            ExportOutcome {
                audio_seconds: out.duration_seconds(),
                render_seconds: started.elapsed().as_secs_f64(),
                guard_engaged: false,
            }
        }
        (ExportSource::InMemory(audio), ExportFormat::Mp3) => {
            let rendered = VocalEngine::render_interleaved(
                &audio.data,
                audio.channels,
                audio.sample_rate,
                params,
                QualityProfile::Render,
            )
            .map_err(|e| mvl_io::Error::InvalidAudio(format!("engine: {e}")))?;
            progress(0.9);
            let out = InterleavedAudio::new(rendered, audio.sample_rate, audio.channels)?;
            mvl_io::mp3::export(path, &out, &mvl_io::Mp3Settings::default())?;
            ExportOutcome {
                audio_seconds: out.duration_seconds(),
                render_seconds: started.elapsed().as_secs_f64(),
                guard_engaged: false,
            }
        }
        (ExportSource::OnDisk(src), ExportFormat::Wav) => {
            stream_render_wav(src, path, params, progress)?
        }
        (ExportSource::OnDisk(src), ExportFormat::Mp3) => {
            // decode once, then the offline path
            let audio = mvl_io::wav::import(src)?;
            let rendered = VocalEngine::render_interleaved(
                &audio.data,
                audio.channels,
                audio.sample_rate,
                params,
                QualityProfile::Render,
            )
            .map_err(|e| mvl_io::Error::InvalidAudio(format!("engine: {e}")))?;
            progress(0.9);
            let out = InterleavedAudio::new(rendered, audio.sample_rate, audio.channels)?;
            mvl_io::mp3::export(path, &out, &mvl_io::Mp3Settings::default())?;
            ExportOutcome {
                audio_seconds: out.duration_seconds(),
                render_seconds: started.elapsed().as_secs_f64(),
                guard_engaged: false,
            }
        }
    };
    progress(1.0);
    Ok(outcome)
}

/// Chunk size for the streaming export path.
const STREAM_FRAMES: usize = 8192;

/// Stream an f32 WAV from disk through per-channel render engines into a
/// new f32 WAV on disk. RAM stays O(chunk).
fn stream_render_wav(
    src: &Path,
    dst: &Path,
    params: VocalParams,
    progress: &dyn Fn(f32),
) -> mvl_io::Result<ExportOutcome> {
    let started = Instant::now();
    let mut reader = hound::WavReader::open(src)
        .map_err(|e| mvl_io::Error::Wav(format!("{}: {e}", src.display())))?;
    let spec = reader.spec();
    let channels = spec.channels as usize;
    let rate = spec.sample_rate;
    let total_frames = reader.duration() as usize; // frames
    if channels == 0 || total_frames == 0 {
        return Err(mvl_io::Error::InvalidAudio("empty session".into()));
    }

    let out_spec = hound::WavSpec {
        channels: spec.channels,
        sample_rate: rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut writer = hound::WavWriter::create(dst, out_spec)
        .map_err(|e| mvl_io::Error::Wav(format!("{}: {e}", dst.display())))?;

    let mut engines: Vec<VocalEngine> = (0..channels)
        .map(|_| {
            VocalEngine::new(rate, QualityProfile::Render)
                .map_err(|e| mvl_io::Error::InvalidAudio(format!("engine: {e}")))
        })
        .collect::<mvl_io::Result<Vec<_>>>()?;

    let params = params.sanitized();
    let mut samples = reader.samples::<f32>();
    let mut frame_buf: Vec<f32> = Vec::with_capacity(STREAM_FRAMES * channels);
    let mut done = 0usize;
    let mut guard = false;
    loop {
        frame_buf.clear();
        let mut frames_this = 0usize;
        'block: for _ in 0..STREAM_FRAMES {
            for _ in 0..channels {
                match samples.next() {
                    Some(Ok(s)) => frame_buf.push(s),
                    Some(Err(e)) => {
                        return Err(mvl_io::Error::Wav(format!("decode: {e}")));
                    }
                    None => break 'block,
                }
            }
            frames_this += 1;
        }
        if frames_this == 0 {
            break;
        }
        // deinterleave → per-engine process → interleave → write
        let mut outs: Vec<Vec<f32>> = Vec::with_capacity(channels);
        for (c, engine) in engines.iter_mut().enumerate() {
            let chan: Vec<f32> = (0..frames_this)
                .map(|f| frame_buf[f * channels + c])
                .collect();
            let out = engine
                .process(&chan, params)
                .map_err(|e| mvl_io::Error::InvalidAudio(format!("engine: {e}")))?;
            outs.push(out);
        }
        let n = outs.iter().map(Vec::len).max().unwrap_or(0);
        for i in 0..n {
            let mut frame_out = vec![0.0f32; channels];
            for (c, out) in outs.iter().enumerate() {
                frame_out[c] = out.get(i).copied().unwrap_or(0.0);
            }
            for s in frame_out {
                writer
                    .write_sample(s)
                    .map_err(|e| mvl_io::Error::Wav(format!("write: {e}")))?;
            }
        }
        done += frames_this;
        progress((done as f32 / total_frames as f32).clamp(0.0, 1.0));
    }

    // flush engine tails
    let mut tails: Vec<Vec<f32>> = engines
        .iter_mut()
        .map(|e| {
            e.flush()
                .map_err(|er| mvl_io::Error::InvalidAudio(format!("engine: {er}")))
        })
        .collect::<mvl_io::Result<Vec<_>>>()?;
    let n = tails.iter().map(Vec::len).max().unwrap_or(0);
    for i in 0..n {
        let mut frame_out = vec![0.0f32; channels];
        for (c, tail) in tails.iter_mut().enumerate() {
            frame_out[c] = tail.get(i).copied().unwrap_or(0.0);
        }
        for s in frame_out {
            writer
                .write_sample(s)
                .map_err(|e| mvl_io::Error::Wav(format!("write: {e}")))?;
        }
    }
    for e in &engines {
        guard |= e.guard_engaged();
    }
    writer
        .finalize()
        .map_err(|e| mvl_io::Error::Wav(format!("finalize: {e}")))?;

    Ok(ExportOutcome {
        audio_seconds: done as f64 / f64::from(rate),
        render_seconds: started.elapsed().as_secs_f64(),
        guard_engaged: guard,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_vocal() -> InterleavedAudio {
        // 0.25 s voiced-ish stack at 48 kHz
        let rate = 48_000u32;
        let data = (0..12_000)
            .map(|i| {
                let t = i as f64 / f64::from(rate);
                ((2.0 * std::f64::consts::PI * 200.0 * t).sin() * 0.4
                    + (2.0 * std::f64::consts::PI * 400.0 * t).sin() * 0.2) as f32
            })
            .collect();
        InterleavedAudio::new(data, rate, 1).unwrap()
    }

    #[test]
    fn imported_session_shapes() {
        let s = Session::from_imported(tiny_vocal(), "take1.wav".into());
        assert_eq!(s.preview.sample_rate, 48_000);
        assert_eq!(s.duration_secs, 0.25);
        assert_eq!(s.channels, 1);
        assert_eq!(s.display_name, "take1.wav");
        assert_eq!(s.mipmap.frames(), 12_000);
        assert!(matches!(s.export_source, ExportSource::InMemory(_)));
    }

    #[test]
    fn high_rate_import_gets_48k_preview() {
        let rate = 96_000u32;
        let data = vec![0.25f32; 96_000]; // 1 s at 96 kHz
        let audio = InterleavedAudio::new(data, rate, 1).unwrap();
        let s = Session::from_imported(audio, "hi.wav".into());
        assert_eq!(s.preview.sample_rate, PREVIEW_RATE);
        // resampling preserves duration: still ~1 s (≈ 48 000 frames)
        assert!(
            (s.preview.duration_seconds() - 1.0).abs() < 0.01,
            "preview duration {}",
            s.preview.duration_seconds()
        );
        // export source keeps the original rate
        if let ExportSource::InMemory(a) = &s.export_source {
            assert_eq!(a.sample_rate, 96_000);
        } else {
            panic!("must be in-memory");
        }
    }

    #[test]
    fn export_wav_roundtrip_neutral_and_pitched() {
        let dir = std::env::temp_dir().join("mvl-app-session-tests");
        std::fs::create_dir_all(&dir).unwrap();

        // neutral export of a neutral session must be a byte-level copy of
        // the source material (bit-exact bypass)
        let audio = tiny_vocal();
        let s = Session::from_imported(audio.clone(), "t.wav".into());
        let out = dir.join("neutral.wav");
        let calls = std::cell::RefCell::new(Vec::new());
        export_session(&s, VocalParams::neutral(), &out, &|p| {
            calls.borrow_mut().push(p);
        })
        .unwrap();
        assert!(out.exists());
        assert_eq!(calls.borrow().last(), Some(&1.0f32));
        let written = mvl_io::wav::import(&out).unwrap();
        assert_eq!(written.data, audio.data, "neutral export is bit-exact");
        assert_eq!(written.sample_rate, 48_000);

        // pitched export differs but keeps the length
        let out2 = dir.join("pitched.wav");
        export_session(
            &s,
            VocalParams {
                pitch_semitones: 4.0,
                ..VocalParams::neutral()
            },
            &out2,
            &|_| {},
        )
        .unwrap();
        let written2 = mvl_io::wav::import(&out2).unwrap();
        assert_eq!(written2.data.len(), audio.data.len());
        assert_ne!(written2.data, audio.data);
    }

    #[test]
    fn export_mp3_from_memory() {
        let dir = std::env::temp_dir().join("mvl-app-session-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let s = Session::from_imported(tiny_vocal(), "t.wav".into());
        let out = dir.join("out.mp3");
        export_session(&s, VocalParams::neutral(), &out, &|_| {}).unwrap();
        assert!(out.exists() && out.metadata().unwrap().len() > 1000);
        let (back, _) = mvl_io::mp3::import_any(&out).unwrap();
        assert_eq!(back.sample_rate, 48_000);
    }

    #[test]
    fn on_disk_session_streams_to_wav() {
        let dir = std::env::temp_dir().join("mvl-app-session-tests");
        std::fs::create_dir_all(&dir).unwrap();

        // write a 96 kHz "recording" to disk (f32, 0.5 s)
        let rate = 96_000u32;
        let data: Vec<f32> = (0..48_000)
            .map(|i| {
                ((2.0 * std::f64::consts::PI * 220.0 * i as f64 / f64::from(rate)).sin() * 0.3)
                    as f32
            })
            .collect();
        let src = dir.join("session-96k.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: rate,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut w = hound::WavWriter::create(&src, spec).unwrap();
        for s in &data {
            w.write_sample(*s).unwrap();
        }
        w.finalize().unwrap();

        let session = Session::from_recording(&src).unwrap();
        assert_eq!(session.preview.sample_rate, PREVIEW_RATE);
        assert_eq!(session.sample_rate, 96_000);
        assert!((session.duration_secs - 0.5).abs() < 0.01);
        assert!(matches!(session.export_source, ExportSource::OnDisk(_)));

        // neutral streamed export must be bit-exact vs the source file
        let out = dir.join("streamed-out.wav");
        export_session(&session, VocalParams::neutral(), &out, &|_| {}).unwrap();
        let written = mvl_io::wav::import(&out).unwrap();
        assert_eq!(written.data, data, "streamed neutral render is bit-exact");
        assert_eq!(written.sample_rate, 96_000);
    }

    #[test]
    fn format_detection() {
        assert_eq!(
            ExportFormat::from_path(Path::new("a/b.MP3")),
            ExportFormat::Mp3
        );
        assert_eq!(
            ExportFormat::from_path(Path::new("a/b.wav")),
            ExportFormat::Wav
        );
        assert_eq!(ExportFormat::from_path(Path::new("a/b")), ExportFormat::Wav);
    }

    #[test]
    fn realtime_factor() {
        let o = ExportOutcome {
            audio_seconds: 10.0,
            render_seconds: 1.0,
            guard_engaged: false,
        };
        assert!((o.realtime_factor() - 10.0).abs() < 1e-9);
    }
}
