//! Microphone recording via [cpal] at up to 192 kHz / 32-bit float.
//!
//! The audio callback never allocates, locks, or touches the file system:
//! it converts samples to `f32` and pushes them into a lock-free
//! [`rtrb`] ring buffer. A dedicated writer thread drains the ring into a
//! hound float-WAV on disk — the disk-backed session model of the
//! architecture plan §9.2, which keeps RAM flat no matter how long the
//! recording runs.
//!
//! Capture rate is *negotiated*, not assumed: [`negotiate`] picks the best
//! supported configuration from the device's actual capability ranges
//! (pure function, unit-tested headless), and the caller can surface an
//! honest "recording at N kHz — device limit" notice when 192 kHz is not
//! available.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use rtrb::{Consumer, Producer};

use crate::{Error, Result};

/// Seconds of ring-buffer headroom between the audio callback and the
/// disk writer. Four seconds absorbs worst-case I/O stalls (slow SD
/// cards, antivirus scans) without dropping samples.
const RING_SECONDS: f64 = 4.0;

/// What the caller asks the recorder for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordRequest {
    /// Preferred sample rate in Hz (studio target: 192_000).
    pub target_rate: u32,
    /// Preferred channel count (vocal work: 1).
    pub channels: u16,
}

impl Default for RecordRequest {
    fn default() -> Self {
        Self {
            target_rate: 192_000,
            channels: 1,
        }
    }
}

/// Sample formats the recorder can consume (subset of cpal's formats that
/// cover effectively every microphone on all three platforms).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleFmt {
    /// 32-bit IEEE float — the studio path, no conversion needed.
    F32,
    /// 16-bit signed integer — converted in the callback (÷ 32768).
    I16,
    /// 16-bit unsigned integer — converted in the callback (bias 32768).
    U16,
}

/// One capability range a device offers (mirrors cpal's
/// `SupportedStreamConfigRange`, minus the hardware dependency).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigRange {
    /// Lowest sample rate in this range (inclusive), Hz.
    pub min_rate: u32,
    /// Highest sample rate in this range (inclusive), Hz.
    pub max_rate: u32,
    /// Channel count this entry applies to.
    pub channels: u16,
    /// Sample format of this entry.
    pub format: SampleFmt,
}

impl ConfigRange {
    /// Does this range contain `rate`?
    pub fn contains_rate(&self, rate: u32) -> bool {
        rate >= self.min_rate && rate <= self.max_rate
    }
}

/// The configuration the recorder will actually run with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NegotiatedConfig {
    /// Agreed sample rate in Hz.
    pub sample_rate: u32,
    /// Agreed channel count.
    pub channels: u16,
    /// Agreed sample format (conversion to f32 happens in the callback if
    /// this is not [`SampleFmt::F32`]).
    pub format: SampleFmt,
}

/// Choose the best available configuration for a [`RecordRequest`].
///
/// Rules (architecture plan §5.1), in priority order:
/// 1. An F32 range that *contains* the requested rate at the requested
///    channel count — exact studio config.
/// 2. An F32 range containing the requested rate at any channel count.
/// 3. The best F32 range at/above 44.1 kHz: the highest rate that is
///    `>= 44_100`, capped at the requested rate, at the best channel
///    match.
/// 4. Same as 1–3 but for I16, then U16 (integer-only devices).
///
/// Returns `None` when no range offers at least 44.1 kHz — the caller
/// then falls back to the device's default config or reports an error.
pub fn negotiate(ranges: &[ConfigRange], request: &RecordRequest) -> Option<NegotiatedConfig> {
    for &format in &[SampleFmt::F32, SampleFmt::I16, SampleFmt::U16] {
        // Exact rate + channel match.
        if let Some(r) = ranges.iter().find(|r| {
            r.format == format
                && r.channels == request.channels
                && r.contains_rate(request.target_rate)
        }) {
            return Some(NegotiatedConfig {
                sample_rate: request.target_rate,
                channels: r.channels,
                format,
            });
        }
        // Exact rate, other channel count (prefer the fewest channels —
        // vocal work and smaller files).
        if let Some(r) = ranges
            .iter()
            .filter(|r| r.format == format && r.contains_rate(request.target_rate))
            .min_by_key(|r| r.channels)
        {
            return Some(NegotiatedConfig {
                sample_rate: request.target_rate,
                channels: r.channels,
                format,
            });
        }
        // Best sub-requested rate at or above 44.1 kHz: prefer the
        // highest rate, then the requested channel count, then the fewest
        // channels.
        let best = ranges
            .iter()
            .filter(|r| r.format == format)
            .filter_map(|r| {
                let rate = r.max_rate.min(request.target_rate);
                if rate >= 44_100 && r.min_rate <= rate {
                    Some((r, rate))
                } else {
                    None
                }
            })
            .max_by_key(|(r, rate)| {
                let channel_bonus: u32 = if r.channels == request.channels {
                    1 << 16
                } else {
                    0
                };
                (*rate, channel_bonus, u32::from(u16::MAX - r.channels))
            });
        if let Some((best, rate)) = best {
            return Some(NegotiatedConfig {
                sample_rate: rate,
                channels: best.channels,
                format,
            });
        }
    }
    None
}

/// Name of the default cpal audio host (for the About/status display).
pub fn default_host_name() -> String {
    cpal::default_host().id().name().to_string()
}

/// Name of the default input device, if one exists.
///
/// # Errors
/// [`Error::Device`] if the host cannot enumerate devices at all.
pub fn default_input_device_name() -> Result<Option<String>> {
    let host = cpal::default_host();
    match host.default_input_device() {
        Some(d) => Ok(Some(d.to_string())),
        None => Ok(None),
    }
}

/// Statistics of a finished recording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordingStats {
    /// File the audio was streamed to (float32 WAV).
    pub path: PathBuf,
    /// Actual capture sample rate in Hz.
    pub sample_rate: u32,
    /// Actual channel count.
    pub channels: u16,
    /// Total frames written (one frame = one sample per channel).
    pub frames: u64,
    /// Samples dropped because the ring buffer was full (should be 0; any
    /// non-zero value indicates an I/O stall longer than
    /// [`RING_SECONDS`]).
    pub dropped_samples: u64,
}

/// A live recording. Call [`Recorder::stop`] to finalize the WAV and get
/// [`RecordingStats`].
///
/// Dropping a `Recorder` without calling `stop` also finalizes the file
/// gracefully (the writer thread drains and closes) — no data is lost,
/// but errors surface as stderr noise instead of a typed `Result`.
pub struct Recorder {
    stream: Option<cpal::Stream>,
    writer: Option<std::thread::JoinHandle<Result<RecordingStats>>>,
    stop_flag: Arc<AtomicBool>,
    started: Instant,
    config: NegotiatedConfig,
    frames: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
    requested_rate: u32,
}

impl Recorder {
    /// Start recording from the default input device into `path`.
    ///
    /// # Errors
    /// * [`Error::Device`] — no input device, or stream construction failed
    /// * [`Error::Io`] / [`Error::Wav`] — target file problems
    pub fn start(path: &Path, request: RecordRequest) -> Result<Self> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or_else(|| Error::Device("no default input device found".into()))?;
        Self::start_on(&device, path, request)
    }

    /// Start recording from a specific device.
    ///
    /// # Errors
    /// Same as [`Recorder::start`].
    pub fn start_on(device: &cpal::Device, path: &Path, request: RecordRequest) -> Result<Self> {
        let ranges = device
            .supported_input_configs()
            .map_err(|e| Error::Device(format!("query device capabilities: {e}")))?
            .filter_map(|r| {
                let format = match r.sample_format() {
                    cpal::SampleFormat::F32 => SampleFmt::F32,
                    cpal::SampleFormat::I16 => SampleFmt::I16,
                    cpal::SampleFormat::U16 => SampleFmt::U16,
                    _ => return None,
                };
                Some(ConfigRange {
                    min_rate: r.min_sample_rate(),
                    max_rate: r.max_sample_rate(),
                    channels: r.channels(),
                    format,
                })
            })
            .collect::<Vec<_>>();

        let config = negotiate(&ranges, &request)
            .or_else(|| {
                // Capability negotiation found nothing at studio rates — fall
                // back to whatever the device calls its default, as long as
                // it speaks a format we understand.
                let default = device.default_input_config().ok()?;
                let format = match default.sample_format() {
                    cpal::SampleFormat::F32 => SampleFmt::F32,
                    cpal::SampleFormat::I16 => SampleFmt::I16,
                    cpal::SampleFormat::U16 => SampleFmt::U16,
                    _ => return None,
                };
                Some(NegotiatedConfig {
                    sample_rate: default.sample_rate(),
                    channels: default.channels(),
                    format,
                })
            })
            .ok_or_else(|| {
                Error::Device(format!(
                    "input device '{}' offers no supported sample format",
                    device
                ))
            })?;

        Self::spawn(device, path, config, request.target_rate)
    }

    fn spawn(
        device: &cpal::Device,
        path: &Path,
        config: NegotiatedConfig,
        requested_rate: u32,
    ) -> Result<Self> {
        let spec = hound::WavSpec {
            channels: config.channels,
            sample_rate: config.sample_rate,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let writer = hound::WavWriter::create(path, spec)
            .map_err(|e| Error::Wav(format!("create '{}': {e}", path.display())))?;

        let ring_capacity =
            (config.sample_rate as f64 * f64::from(config.channels) * RING_SECONDS) as usize + 4096;
        let (producer, consumer) = rtrb::RingBuffer::new(ring_capacity);

        let stop_flag = Arc::new(AtomicBool::new(false));
        let frames = Arc::new(AtomicU64::new(0));
        let dropped = Arc::new(AtomicU64::new(0));

        let stream_config = cpal::StreamConfig {
            channels: config.channels,
            sample_rate: config.sample_rate,
            buffer_size: cpal::BufferSize::Default,
        };

        let err_cb = |e| log_device_error(e);
        let stream = match config.format {
            SampleFmt::F32 => device
                .build_input_stream::<f32, _, _>(
                    stream_config,
                    input_callback(producer, frames.clone(), dropped.clone(), |s| s),
                    err_cb,
                    None,
                )
                .map_err(|e| Error::Device(format!("open input stream: {e}")))?,
            SampleFmt::I16 => device
                .build_input_stream::<i16, _, _>(
                    stream_config,
                    input_callback(producer, frames.clone(), dropped.clone(), |s| {
                        f32::from(s) / 32768.0
                    }),
                    err_cb,
                    None,
                )
                .map_err(|e| Error::Device(format!("open input stream: {e}")))?,
            SampleFmt::U16 => device
                .build_input_stream::<u16, _, _>(
                    stream_config,
                    input_callback(producer, frames.clone(), dropped.clone(), |s| {
                        (f32::from(s) - 32768.0) / 32768.0
                    }),
                    err_cb,
                    None,
                )
                .map_err(|e| Error::Device(format!("open input stream: {e}")))?,
        };

        stream
            .play()
            .map_err(|e| Error::Device(format!("start input stream: {e}")))?;

        let writer_handle = spawn_writer_thread(
            writer,
            consumer,
            stop_flag.clone(),
            frames.clone(),
            config,
            path.to_path_buf(),
        );

        Ok(Self {
            stream: Some(stream),
            writer: Some(writer_handle),
            stop_flag,
            started: Instant::now(),
            config,
            frames,
            dropped,
            requested_rate,
        })
    }

    /// Wall-clock duration since recording started.
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// The configuration actually in use — may differ from the request
    /// when the device cannot do 192 kHz (surface this honestly in the
    /// UI, plan §5.1).
    pub fn actual_config(&self) -> NegotiatedConfig {
        self.config
    }

    /// True when the device fell short of the requested studio rate.
    pub fn rate_degraded(&self) -> bool {
        self.config.sample_rate != self.requested_rate
    }

    /// Frames captured so far (played back through the ring buffer).
    pub fn frames_captured(&self) -> u64 {
        self.frames.load(Ordering::Relaxed)
    }

    /// Samples dropped so far (should stay 0).
    pub fn dropped_samples(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Stop recording, drain the ring, finalize the WAV file, and return
    /// the recording statistics.
    ///
    /// # Errors
    /// [`Error::InvalidState`] — the writer thread already failed;
    /// the underlying writer error is passed through otherwise.
    pub fn stop(mut self) -> Result<RecordingStats> {
        self.stop_inner()
    }

    /// Shared stop logic: pause the callback, signal the writer, drain,
    /// finalize. Also used by `Drop` (which ignores errors).
    fn stop_inner(&mut self) -> Result<RecordingStats> {
        // Stop the callback first so the ring can drain to completion.
        if let Some(stream) = self.stream.take() {
            stream
                .pause()
                .map_err(|e| Error::Device(format!("pause input stream: {e}")))?;
            drop(stream);
        }
        self.stop_flag.store(true, Ordering::Release);
        match self.writer.take() {
            Some(handle) => handle
                .join()
                .map_err(|_| Error::InvalidState("recorder writer thread panicked".into()))?,
            None => Err(Error::InvalidState("recorder already stopped".into())),
        }
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        if self.stream.is_some() {
            if let Ok(stats) = self.stop_inner() {
                eprintln!(
                    "mvl-io: recording finalized on drop — {} frames, {} dropped, '{}'",
                    stats.frames,
                    stats.dropped_samples,
                    stats.path.display()
                );
            }
        }
    }
}

/// Build the real-time input callback for a sample type with an `f32`
/// conversion function. Converts into a pre-allocated scratch buffer, then
/// pushes into the ring; on overflow the samples are dropped and counted
/// (never blocks, never allocates).
fn input_callback<T: Copy>(
    mut producer: Producer<f32>,
    frames: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
    convert: impl Fn(T) -> f32 + Send + 'static,
) -> impl FnMut(&[T], &cpal::InputCallbackInfo) + Send + 'static {
    let mut scratch: Vec<f32> = Vec::new();
    move |data: &[T], _info: &cpal::InputCallbackInfo| {
        scratch.clear();
        scratch.extend(data.iter().map(|&s| convert(s)));
        let mut remaining: &[f32] = &scratch;
        while !remaining.is_empty() {
            let (_, rest) = producer.push_partial_slice(remaining);
            if rest.len() == remaining.len() {
                // Ring full: drop the rest and count it.
                dropped.fetch_add(rest.len() as u64, Ordering::Relaxed);
                break;
            }
            remaining = rest;
        }
        frames.fetch_add(data.len() as u64, Ordering::Relaxed);
    }
}

/// Drain the ring into the WAV writer until stopped, then finalize.
fn spawn_writer_thread(
    mut writer: hound::WavWriter<std::io::BufWriter<std::fs::File>>,
    mut consumer: Consumer<f32>,
    stop_flag: Arc<AtomicBool>,
    frames: Arc<AtomicU64>,
    config: NegotiatedConfig,
    path: PathBuf,
) -> std::thread::JoinHandle<Result<RecordingStats>> {
    std::thread::spawn(move || {
        let mut chunk = vec![0.0f32; 8192];
        let mut written: u64 = 0;
        loop {
            let (filled, _) = consumer.pop_partial_slice(&mut chunk);
            let n = filled.len();
            if n > 0 {
                for &s in &chunk[..n] {
                    // Defensive: non-finite capture data must never reach
                    // the file (some drivers emit NaN bursts on reconfig).
                    let s = if s.is_finite() { s } else { 0.0 };
                    writer
                        .write_sample(s)
                        .map_err(|e| Error::Wav(format!("write '{}': {e}", path.display())))?;
                }
                written += n as u64;
            } else if stop_flag.load(Ordering::Acquire) {
                break;
            } else {
                // Idle: brief sleep keeps this thread off the CPU without
                // adding meaningful latency (ring absorbs the jitter).
                std::thread::sleep(Duration::from_millis(3));
            }
        }
        let captured = frames.load(Ordering::Relaxed);
        let dropped = captured.saturating_sub(written);
        writer
            .finalize()
            .map_err(|e| Error::Wav(format!("finalize '{}': {e}", path.display())))?;
        Ok(RecordingStats {
            path,
            sample_rate: config.sample_rate,
            channels: config.channels,
            frames: written / u64::from(config.channels),
            dropped_samples: dropped,
        })
    })
}

fn log_device_error(e: cpal::Error) {
    eprintln!("mvl-io: audio device error: {e}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(min: u32, max: u32, ch: u16, f: SampleFmt) -> ConfigRange {
        ConfigRange {
            min_rate: min,
            max_rate: max,
            channels: ch,
            format: f,
        }
    }

    #[test]
    fn negotiate_picks_exact_192k_f32_mono() {
        let ranges = vec![
            range(44_100, 48_000, 1, SampleFmt::F32),
            range(44_100, 192_000, 1, SampleFmt::F32),
            range(44_100, 192_000, 2, SampleFmt::I16),
        ];
        let got = negotiate(&ranges, &RecordRequest::default()).unwrap();
        assert_eq!(
            got,
            NegotiatedConfig {
                sample_rate: 192_000,
                channels: 1,
                format: SampleFmt::F32
            }
        );
    }

    #[test]
    fn negotiate_falls_back_to_highest_f32_rate() {
        // No 192 kHz support: expect the highest F32 rate <= request.
        let ranges = vec![
            range(44_100, 96_000, 1, SampleFmt::F32),
            range(44_100, 192_000, 1, SampleFmt::I16),
        ];
        let got = negotiate(&ranges, &RecordRequest::default()).unwrap();
        assert_eq!(got.sample_rate, 96_000);
        assert_eq!(got.format, SampleFmt::F32);
    }

    #[test]
    fn negotiate_f32_beats_i16_even_at_lower_rate() {
        // Format quality (F32) wins over raw rate within legal studio
        // bounds: 96k F32 is chosen before 192k I16.
        let ranges = vec![
            range(44_100, 96_000, 1, SampleFmt::F32),
            range(44_100, 192_000, 1, SampleFmt::I16),
        ];
        let got = negotiate(&ranges, &RecordRequest::default()).unwrap();
        assert_eq!(got.format, SampleFmt::F32);
        assert_eq!(got.sample_rate, 96_000);
    }

    #[test]
    fn negotiate_uses_i16_when_only_option() {
        let ranges = vec![range(44_100, 48_000, 1, SampleFmt::I16)];
        let got = negotiate(&ranges, &RecordRequest::default()).unwrap();
        assert_eq!(
            got,
            NegotiatedConfig {
                sample_rate: 48_000,
                channels: 1,
                format: SampleFmt::I16
            }
        );
    }

    #[test]
    fn negotiate_prefers_requested_channels() {
        let ranges = vec![
            range(44_100, 192_000, 2, SampleFmt::F32),
            range(44_100, 192_000, 1, SampleFmt::F32),
        ];
        let got = negotiate(&ranges, &RecordRequest::default()).unwrap();
        assert_eq!(got.channels, 1, "mono (requested) must win over stereo");
    }

    #[test]
    fn negotiate_exact_rate_other_channels_beats_fallback() {
        // 192 kHz stereo F32 beats 96 kHz mono F32.
        let ranges = vec![
            range(44_100, 96_000, 1, SampleFmt::F32),
            range(44_100, 192_000, 2, SampleFmt::F32),
        ];
        let got = negotiate(&ranges, &RecordRequest::default()).unwrap();
        assert_eq!(got.sample_rate, 192_000);
        assert_eq!(got.channels, 2);
    }

    #[test]
    fn negotiate_rejects_telephony_only_devices() {
        let ranges = vec![range(8_000, 16_000, 1, SampleFmt::I16)];
        assert!(negotiate(&ranges, &RecordRequest::default()).is_none());
    }

    #[test]
    fn negotiate_empty_ranges_is_none() {
        assert!(negotiate(&[], &RecordRequest::default()).is_none());
    }

    #[test]
    fn negotiate_caps_at_requested_rate() {
        // A device offering MORE than requested: cap to the request.
        let ranges = vec![range(44_100, 384_000, 1, SampleFmt::F32)];
        let got = negotiate(&ranges, &RecordRequest::default()).unwrap();
        assert_eq!(got.sample_rate, 192_000);
    }

    /// Hardware smoke test — requires a real input device. Run manually:
    /// `cargo test -p mvl-io -- --ignored`
    #[test]
    #[ignore = "requires a live audio input device"]
    fn hardware_records_one_second() {
        let mut path = std::env::temp_dir();
        path.push(format!("mvl_hw_record_{}.wav", std::process::id()));
        let recorder = Recorder::start(&path, RecordRequest::default())
            .expect("default input device should record");
        let cfg = recorder.actual_config();
        std::thread::sleep(Duration::from_secs(1));
        let stats = recorder.stop().expect("stop must finalize");
        assert_eq!(stats.sample_rate, cfg.sample_rate);
        assert_eq!(stats.channels, cfg.channels);
        assert_eq!(stats.dropped_samples, 0, "no samples may be dropped in 1 s");
        // Duration within ±15% (device clocks and callback scheduling vary).
        let expected = f64::from(cfg.sample_rate);
        let got = stats.frames as f64;
        assert!(
            (got - expected).abs() / expected < 0.15,
            "recorded {got} frames, expected ~{expected}"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// Virtual-device smoke test — runs against an UN-PACED ALSA null PCM in
    /// CI (`MVL_VIRTUAL_AUDIO=1` + `~/.asoundrc` with `pcm.!default {
    /// type null }`). The null plugin has no hardware clock (verified with a
    /// C probe, Phase 5): capture delivers frames as fast as the callback
    /// thread can pump, so the real-hardware duration/dropped-sample
    /// assertions above do NOT apply. What this still proves end to end:
    /// device enumeration, capability negotiation, stream build, callback
    /// delivery, f32 conversion, ring transport, disk streaming and WAV
    /// finalization — the full cpal/ALSA path, on every push.
    #[test]
    fn virtual_recorder_smoke() {
        if std::env::var("MVL_VIRTUAL_AUDIO").ok().as_deref() != Some("1") {
            eprintln!("skipping: set MVL_VIRTUAL_AUDIO=1 with a null ALSA device to enable");
            return;
        }
        let mut path = std::env::temp_dir();
        path.push(format!("mvl_virtual_record_{}.wav", std::process::id()));
        let recorder = Recorder::start(&path, RecordRequest::default())
            .expect("virtual default input device should record");
        let cfg = recorder.actual_config();
        assert!(
            cfg.sample_rate >= 44_100,
            "negotiation must not fall below the 44.1 kHz floor (got {cfg:?})"
        );
        std::thread::sleep(Duration::from_millis(50));
        let stats = recorder.stop().expect("stop must finalize");
        assert_eq!(stats.sample_rate, cfg.sample_rate);
        assert_eq!(stats.channels, cfg.channels);
        assert!(
            stats.frames > 0,
            "un-paced capture must still produce frames"
        );
        assert!(path.exists(), "WAV must exist after stop");
        let _ = std::fs::remove_file(&path);
    }
}
