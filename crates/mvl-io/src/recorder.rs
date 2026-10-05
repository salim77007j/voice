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

/// Sample formats the recorder can consume (every format cpal exposes on
/// the three desktop platforms — v1.0.0 only handled F32/I16/U16 and
/// *refused to record* on devices that offered anything else, e.g. ALSA
/// S32-only hardware or exclusive-mode WASAPI i32 endpoints).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SampleFmt {
    /// 32-bit IEEE float — the studio path, no conversion needed.
    F32,
    /// 16-bit signed integer — converted in the callback (÷ 32768).
    I16,
    /// 32-bit signed integer — converted in the callback (÷ 2³¹).
    I32,
    /// 16-bit unsigned integer — converted in the callback (bias 32768).
    U16,
    /// 32-bit unsigned integer — converted in the callback (bias 2³¹).
    U32,
    /// 8-bit unsigned integer (RIFF convention) — bias 128.
    U8,
}

impl SampleFmt {
    /// Every format, in negotiation-preference order.
    pub const ALL: [SampleFmt; 6] = [
        SampleFmt::F32,
        SampleFmt::I16,
        SampleFmt::I32,
        SampleFmt::U16,
        SampleFmt::U32,
        SampleFmt::U8,
    ];

    /// Map a cpal sample format onto ours, if supported.
    #[must_use]
    pub fn from_cpal(f: cpal::SampleFormat) -> Option<Self> {
        match f {
            cpal::SampleFormat::F32 => Some(Self::F32),
            cpal::SampleFormat::I16 => Some(Self::I16),
            cpal::SampleFormat::I32 => Some(Self::I32),
            cpal::SampleFormat::U16 => Some(Self::U16),
            cpal::SampleFormat::U32 => Some(Self::U32),
            cpal::SampleFormat::U8 => Some(Self::U8),
            _ => None,
        }
    }
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
/// 4. Same as 1–3 for every other supported format, in preference order
///    (I16, I32, U16, U32, U8) — integer-only devices must still record.
///
/// Returns `None` when no range offers at least 44.1 kHz — the caller
/// then falls back to the device's default config or reports an error.
pub fn negotiate(ranges: &[ConfigRange], request: &RecordRequest) -> Option<NegotiatedConfig> {
    for &format in &SampleFmt::ALL {
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

/// Hard cap on negotiation-ladder length — pathological hosts can report
/// dozens of ranges; more than this many *stream-open* attempts would
/// waste time on devices that are clearly refusing everything.
const MAX_LADDER: usize = 24;

/// Build the ordered list of stream configurations to *actually try* on
/// one device (v1.1.1 BUG 1 fix).
///
/// Negotiating from the reported ranges alone is not enough: a host can
/// accept a capability query and still refuse the stream open (WASAPI
/// shared mode wants the mix format; exclusive-mode hardware wants the
/// exact format). The ladder therefore walks from the ideal config down
/// to "anything this device reports", and [`Recorder::start_on`]
/// attempts an *open* for each rung until one plays:
///
/// 0. the classic [`negotiate`] pick (best studio config, v1.1.0 rules)
/// 1. requested rate × requested channels
/// 2. requested rate × mono
/// 3. 48 kHz × requested channels
/// 4. 48 kHz × mono
/// 5. 44.1 kHz × requested channels
/// 6. 44.1 kHz × mono
/// 7. the device's own default config (on WASAPI this is the shared-mode
///    mix format, which the engine always accepts)
/// 8. every reported range's best and minimum representatives
///
/// Within rungs 1–6 every sample format is tried in [`SampleFmt::ALL`]
/// preference order, but only combinations the device actually reports.
/// The result is deduplicated and capped at [`MAX_LADDER`] entries.
#[must_use]
pub fn negotiation_ladder(
    ranges: &[ConfigRange],
    request: &RecordRequest,
    device_default: Option<NegotiatedConfig>,
) -> Vec<NegotiatedConfig> {
    let mut ladder: Vec<NegotiatedConfig> = Vec::new();
    let push = |cfg: NegotiatedConfig, ladder: &mut Vec<NegotiatedConfig>| {
        if !ladder.contains(&cfg) && ladder.len() < MAX_LADDER {
            ladder.push(cfg);
        }
    };

    // 0) classic negotiation pick
    if let Some(cfg) = negotiate(ranges, request) {
        push(cfg, &mut ladder);
    }
    // 1..=6) explicit rate × channel rungs, formats in preference order
    let mono = 1;
    for (rate, channels) in [
        (request.target_rate, request.channels),
        (request.target_rate, mono),
        (48_000, request.channels),
        (48_000, mono),
        (44_100, request.channels),
        (44_100, mono),
    ] {
        for &format in &SampleFmt::ALL {
            if ranges
                .iter()
                .any(|r| r.format == format && r.channels == channels && r.contains_rate(rate))
            {
                push(
                    NegotiatedConfig {
                        sample_rate: rate,
                        channels,
                        format,
                    },
                    &mut ladder,
                );
            }
        }
    }
    // 7) the device's default config, trusted as-is
    if let Some(cfg) = device_default {
        push(cfg, &mut ladder);
    }
    // 8) every reported range: best (max) rate first, then its floor
    for r in ranges {
        push(
            NegotiatedConfig {
                sample_rate: r.max_rate,
                channels: r.channels,
                format: r.format,
            },
            &mut ladder,
        );
        push(
            NegotiatedConfig {
                sample_rate: r.min_rate,
                channels: r.channels,
                format: r.format,
            },
            &mut ladder,
        );
    }
    ladder
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
    /// Start recording, trying every input device on every host until
    /// one actually opens a stream (BUG 1 fix).
    ///
    /// Fallback order (via [`crate::devices`]):
    /// 1. the default host's default input device,
    /// 2. default input devices of other hosts,
    /// 3. every other input device.
    ///
    /// Each candidate is *fully opened* (capability query → negotiate →
    /// stream build → `play()`); the first candidate that completes wins.
    /// Devices that fail are skipped, and the failure that got the
    /// furthest is preserved for the error message when they all fail.
    ///
    /// # Errors
    /// * [`Error::Device`] — no input device opened (message includes a
    ///   machine inventory + platform-specific hints)
    /// * [`Error::Io`] / [`Error::Wav`] — target file problems
    pub fn start(path: &Path, request: RecordRequest) -> Result<Self> {
        let inv = crate::devices::list();
        let default_host = cpal::default_host().id().name().to_string();
        let candidates = crate::devices::fallback_order(&inv.inputs, &default_host);

        let mut last_err: Option<Error> = None;
        let mut attempted = 0usize;
        let mut all_access_denied = true;
        for cand in candidates {
            let dev = match crate::devices::open_by_id(&cand.id()) {
                Ok(d) => d,
                Err(e) => {
                    last_err = Some(e);
                    continue;
                }
            };
            match Self::start_on(&dev, path, request) {
                Ok(rec) => return Ok(rec),
                Err(e) => {
                    eprintln!(
                        "mvl-io: input device '{}' failed to open, trying next: {e}",
                        cand.label()
                    );
                    attempted += 1;
                    if !crate::consent::is_access_denied(&e.to_string()) {
                        all_access_denied = false;
                    }
                    last_err = Some(e);
                }
            }
        }
        // Total failure: don't leave a header-only session WAV behind —
        // each attempt's WavWriter truncated it, but nothing was captured.
        let _ = std::fs::remove_file(path);
        if attempted > 0 && all_access_denied {
            // Every device returned E_ACCESSDENIED: this is the OS privacy
            // gate, not a per-device or format problem — say exactly that
            // (v1.1.1 BUG 1: on Windows, include the live consent-store
            // state and the exact settings to change).
            return Err(crate::consent::access_denied_error(&format!(
                "tried {attempted} input device(s)"
            )));
        }
        Err(match last_err {
            Some(e) if inv.inputs.len() <= 1 => e,
            // Multiple candidates all failed: wrap with context so the
            // user knows *everything* was tried, not just one device.
            Some(e) => Error::Device(format!(
                "tried {} input device(s); last failure: {e}",
                inv.inputs.len()
            )),
            None => crate::devices::no_input_device_error(&inv),
        })
    }

    /// Start recording from a specific device.
    ///
    /// Walks the full [`negotiation_ladder`] and attempts an actual
    /// stream open for each rung until one plays (v1.1.1 BUG 1 fix):
    /// a capability query alone cannot predict what the stream API will
    /// accept, so negotiation now means *opening*, not just choosing.
    /// Every failed attempt is logged with its rung and reason so users
    /// can diagnose device problems from the console output.
    ///
    /// # Errors
    /// Same as [`Recorder::start`].
    pub fn start_on(device: &cpal::Device, path: &Path, request: RecordRequest) -> Result<Self> {
        let ranges = device
            .supported_input_configs()
            .map_err(|e| Error::Device(format!("query device capabilities: {e}")))?
            .filter_map(|r| {
                let format = SampleFmt::from_cpal(r.sample_format())?;
                Some(ConfigRange {
                    min_rate: r.min_sample_rate(),
                    max_rate: r.max_sample_rate(),
                    channels: r.channels(),
                    format,
                })
            })
            .collect::<Vec<_>>();

        let device_default = device.default_input_config().ok().and_then(|d| {
            SampleFmt::from_cpal(d.sample_format()).map(|format| NegotiatedConfig {
                sample_rate: d.sample_rate(),
                channels: d.channels(),
                format,
            })
        });

        let ladder = negotiation_ladder(&ranges, &request, device_default);
        if ladder.is_empty() {
            return Err(Error::Device(format!(
                "input device '{}' offers no supported sample format",
                device
            )));
        }

        let total = ladder.len();
        let mut last_err: Option<Error> = None;
        for (i, config) in ladder.into_iter().enumerate() {
            match Self::spawn(device, path, config, request.target_rate) {
                Ok(rec) => {
                    if i > 0 {
                        eprintln!(
                            "mvl-io: '{}' opened on negotiation rung {}/{}: \
                             {} Hz, {} ch, {:?} (requested {} Hz)",
                            device,
                            i + 1,
                            total,
                            config.sample_rate,
                            config.channels,
                            config.format,
                            request.target_rate
                        );
                    }
                    return Ok(rec);
                }
                Err(e) => {
                    eprintln!(
                        "mvl-io: '{}' rung {}/{} ({} Hz, {} ch, {:?}) failed: {e}",
                        device,
                        i + 1,
                        total,
                        config.sample_rate,
                        config.channels,
                        config.format
                    );
                    let denied = crate::consent::is_access_denied(&e.to_string());
                    last_err = Some(e);
                    if denied {
                        // The OS privacy gate blocks every format on this
                        // device identically — stop hammering it and let
                        // the caller's classification take over.
                        eprintln!(
                            "mvl-io: access denied by the OS privacy gate; \
                             skipping remaining formats on this device"
                        );
                        break;
                    }
                }
            }
        }
        Err(last_err.expect("non-empty ladder ran at least one attempt"))
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
            SampleFmt::I32 => device
                .build_input_stream::<i32, _, _>(
                    stream_config,
                    input_callback(producer, frames.clone(), dropped.clone(), |s| {
                        s as f32 / 2147483648.0
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
            SampleFmt::U32 => device
                .build_input_stream::<u32, _, _>(
                    stream_config,
                    input_callback(producer, frames.clone(), dropped.clone(), |s| {
                        (s as f32 - 2147483648.0) / 2147483648.0
                    }),
                    err_cb,
                    None,
                )
                .map_err(|e| Error::Device(format!("open input stream: {e}")))?,
            SampleFmt::U8 => device
                .build_input_stream::<u8, _, _>(
                    stream_config,
                    input_callback(producer, frames.clone(), dropped.clone(), |s| {
                        (f32::from(s) - 128.0) / 128.0
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

    // ---- negotiation ladder (v1.1.1 BUG 1) ---------------------------------

    fn cfg(rate: u32, ch: u16, f: SampleFmt) -> NegotiatedConfig {
        NegotiatedConfig {
            sample_rate: rate,
            channels: ch,
            format: f,
        }
    }

    #[test]
    fn ladder_starts_with_classic_pick_then_requested_rung() {
        let ranges = vec![
            range(44_100, 192_000, 1, SampleFmt::F32),
            range(44_100, 48_000, 1, SampleFmt::I16),
        ];
        let ladder = negotiation_ladder(&ranges, &RecordRequest::default(), None);
        assert_eq!(
            ladder[0],
            cfg(192_000, 1, SampleFmt::F32),
            "rung 0 = negotiate()"
        );
        // rung 1 (192k × 1ch): F32 already pushed; I16 is *not reported*
        // at 192 kHz, so nothing else at this rate may appear.
        assert!(
            !ladder.contains(&cfg(192_000, 1, SampleFmt::I16)),
            "unreported combos must never be attempted"
        );
        // next new rung: 48 kHz × F32 (rung 3), before 44.1 kHz rungs.
        assert_eq!(ladder[1], cfg(48_000, 1, SampleFmt::F32));
        let rates: Vec<u32> = ladder.iter().map(|c| c.sample_rate).collect();
        let pos48 = rates.iter().position(|&r| r == 48_000).unwrap();
        let pos441 = rates.iter().position(|&r| r == 44_100).unwrap();
        assert!(
            pos48 < pos441,
            "48 kHz rungs must come before 44.1 kHz: {rates:?}"
        );
    }

    #[test]
    fn ladder_puts_device_default_after_44k_rungs() {
        let ranges = vec![range(8_000, 16_000, 1, SampleFmt::I16)]; // telephony only
        let default = Some(cfg(16_000, 1, SampleFmt::I16));
        let ladder = negotiation_ladder(&ranges, &RecordRequest::default(), default);
        assert_eq!(
            ladder[0],
            cfg(16_000, 1, SampleFmt::I16),
            "default is rung 7"
        );
        // No 44.1/48/192 rung is reported, so only rung 8's floor
        // representative (8 kHz — better than refusing) follows.
        assert_eq!(ladder.len(), 2);
        assert_eq!(ladder[1], cfg(8_000, 1, SampleFmt::I16));
    }

    #[test]
    fn ladder_tries_mono_when_only_mono_exists() {
        // Requested stereo, device is mono-only: the mono rungs must carry
        // the only usable config.
        let ranges = vec![range(44_100, 44_100, 1, SampleFmt::I16)];
        let req = RecordRequest {
            channels: 2,
            ..RecordRequest::default()
        };
        let ladder = negotiation_ladder(&ranges, &req, None);
        assert!(ladder.contains(&cfg(44_100, 1, SampleFmt::I16)));
        // No stereo config may appear (none is reported).
        assert!(ladder.iter().all(|c| c.channels == 1));
    }

    #[test]
    fn ladder_trusts_device_default_even_unreported() {
        // WASAPI case: ranges can come back empty-ish while the default
        // config (the shared-mode mix format) is the one that opens.
        let default = Some(cfg(48_000, 2, SampleFmt::F32));
        let ladder = negotiation_ladder(&[], &RecordRequest::default(), default);
        assert_eq!(ladder, vec![cfg(48_000, 2, SampleFmt::F32)]);
    }

    #[test]
    fn ladder_empty_when_nothing_reported_and_no_default() {
        assert!(negotiation_ladder(&[], &RecordRequest::default(), None).is_empty());
    }

    #[test]
    fn ladder_is_deduplicated() {
        let ranges = vec![
            range(44_100, 48_000, 1, SampleFmt::F32),
            range(48_000, 48_000, 1, SampleFmt::F32), // same effective configs
        ];
        let ladder = negotiation_ladder(
            &ranges,
            &RecordRequest {
                target_rate: 48_000,
                channels: 1,
            },
            None,
        );
        let mut seen = std::collections::HashSet::new();
        for c in &ladder {
            assert!(seen.insert(*c), "duplicate rung {c:?} in {ladder:?}");
        }
    }

    #[test]
    fn ladder_includes_reported_range_representatives_last() {
        // An exotic device reporting 8 kHz-only I16 (below studio rate):
        // negotiate() refuses it, rungs 1–6 skip it (not 44.1+), but
        // rung 8 must still offer it — an 8 kHz capture beats no capture.
        let ranges = vec![range(8_000, 8_000, 1, SampleFmt::I16)];
        let ladder = negotiation_ladder(&ranges, &RecordRequest::default(), None);
        assert_eq!(ladder.last(), Some(&cfg(8_000, 1, SampleFmt::I16)));
    }

    #[test]
    fn ladder_caps_at_max_entries() {
        // Pathological host reporting 40 disjoint ranges.
        let ranges: Vec<ConfigRange> = (0..40)
            .map(|i| {
                range(
                    8_000 + i * 100,
                    8_100 + i * 100,
                    1 + (i % 8) as u16,
                    SampleFmt::F32,
                )
            })
            .collect();
        let ladder = negotiation_ladder(&ranges, &RecordRequest::default(), None);
        assert!(
            ladder.len() <= super::MAX_LADDER,
            "ladder too long: {}",
            ladder.len()
        );
    }

    #[test]
    fn ladder_prefers_f32_over_i16_within_a_rung() {
        let ranges = vec![
            range(48_000, 48_000, 1, SampleFmt::I16),
            range(48_000, 48_000, 1, SampleFmt::F32),
        ];
        let ladder = negotiation_ladder(&ranges, &RecordRequest::default(), None);
        let first48 = ladder.iter().find(|c| c.sample_rate == 48_000).unwrap();
        assert_eq!(first48.format, SampleFmt::F32);
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
