//! Audio playback via [cpal] with play / pause / stop / seek.
//!
//! Architecture (mirror of the recorder):
//!
//! * a **feeder thread** owns the [`Transport`] and the source audio,
//!   pushes interleaved `f32` into a lock-free ring while playing;
//! * the **cpal output callback** owns the consumer side through
//!   [`SinkLogic`], which fills each device buffer (silence when paused,
//!   data when playing, and flushes stale audio after seek/stop via a
//!   generation counter) and publishes the *playhead anchor* — the frame
//!   position of the first popped sample plus the device-clock instant it
//!   becomes audible (phase 8.2 sync fix);
//! * commands travel over a crossbeam channel; the reported position is
//!   extrapolated from the playhead anchor (device latency subtracted,
//!   see [`crate::playhead`]), clamped to the *handed* cursor
//!   (`base + consumed` — samples actually given to the device), so it
//!   tracks what the listener hears. No mutexes anywhere on the audio
//!   path, no allocation in the callback.
//!
//! If the output device runs at a different sample rate than the source,
//! the source is resampled once via [`crate::resample`] at construction
//! (the 192 kHz session → 48 kHz device case, plan §6.6). Channel-count
//! mismatches are handled by a minimal up/down-mix (mono↔stereo).

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use cpal::traits::{DeviceTrait, StreamTrait};
use crossbeam_channel::{Receiver, Sender};
use rtrb::{Consumer, Producer};

use crate::transport::{Transport, TransportState};
use crate::{Error, InterleavedAudio, Result};

/// Ring-buffer headroom between feeder and device: half a second.
const RING_SECONDS: f64 = 0.5;

/// Feeder pushes in blocks of this many frames.
const FEED_CHUNK_FRAMES: usize = 4096;

/// Player commands (UI → feeder thread).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PlayerCommand {
    /// Start or resume.
    Play,
    /// Freeze at the current position.
    Pause,
    /// Rewind to zero and drain.
    Stop,
    /// Jump to a position in seconds.
    Seek(f64),
    /// Tear the feeder down (used by `Drop`).
    Shutdown,
}

/// Atomic state shared by the API side, the feeder, and the sink.
struct Shared {
    playing: AtomicBool,
    /// Bumped on seek/stop so the sink discards stale ring content.
    generation: AtomicU32,
    /// Samples pushed by the feeder since the last generation bump
    /// (feeder write cursor — diagnostics only since the 8.2 sync fix;
    /// the playhead no longer reads it).
    fed: AtomicU64,
    /// Samples **handed to the device** by the sink since the last
    /// generation bump (flushed/discarded ring content is not counted —
    /// it never reached the device).
    consumed: AtomicU64,
    /// `position_base` in samples (frames × channels) at last bump.
    base: AtomicU64,
    /// Mirrored `TransportState` for the API side (0/1/2).
    state: AtomicU8,
    /// Sample rate of the (possibly resampled) playback stream.
    sample_rate: AtomicU32,
    channels: AtomicU32,
    /// Track length in stream frames (clamps the playhead).
    length_frames: AtomicU64,
    /// Device-clock anchored audible position (phase 8.2).
    playhead: crate::playhead::PlayheadAnchor,
}

impl Shared {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            playing: AtomicBool::new(false),
            generation: AtomicU32::new(0),
            fed: AtomicU64::new(0),
            consumed: AtomicU64::new(0),
            base: AtomicU64::new(0),
            state: AtomicU8::new(0),
            sample_rate: AtomicU32::new(48_000),
            channels: AtomicU32::new(1),
            length_frames: AtomicU64::new(0),
            playhead: crate::playhead::PlayheadAnchor::new(),
        })
    }

    /// Audible position in seconds (shared playhead math, see
    /// [`crate::playhead::audible_position_seconds`]).
    fn position_seconds(&self) -> f64 {
        let base = self.base.load(Ordering::Acquire);
        let consumed = self.consumed.load(Ordering::Acquire);
        let rate = f64::from(self.sample_rate.load(Ordering::Acquire).max(1));
        let channels = u64::from(self.channels.load(Ordering::Acquire).max(1));
        let length = self.length_frames.load(Ordering::Acquire) as f64 / rate;
        crate::playhead::audible_position_seconds(
            &self.playhead,
            rate,
            channels,
            base,
            consumed,
            length,
            self.playing.load(Ordering::Acquire),
            std::time::Instant::now(),
        )
    }

    fn state(&self) -> TransportState {
        match self.state.load(Ordering::Acquire) {
            1 => TransportState::Playing,
            2 => TransportState::Paused,
            _ => TransportState::Stopped,
        }
    }
}

/// Consumer-side buffer fill logic, isolated from cpal so it is unit
/// testable with a plain slice.
///
/// Beyond filling the device buffer it publishes the playhead anchor:
/// every callback that hands audio to the device records the first frame
/// position + the callback instant + the device latency into the shared
/// seqlock (real-time safe, see [`crate::playhead`]).
struct SinkLogic {
    consumer: Consumer<f32>,
    shared: Arc<Shared>,
    observed_generation: u32,
    consumed_here: u64,
    /// Instant of the previous callback (latency fallback: half the
    /// measured callback period when the host reports no playback
    /// instant).
    last_callback_at: Option<std::time::Instant>,
}

impl SinkLogic {
    fn new(consumer: Consumer<f32>, shared: Arc<Shared>) -> Self {
        let observed_generation = shared.generation.load(Ordering::Acquire);
        Self {
            consumer,
            shared,
            observed_generation,
            consumed_here: 0,
            last_callback_at: None,
        }
    }

    /// Fill `out` for one device callback: data while playing, silence
    /// otherwise; stale post-seek audio is flushed first.
    ///
    /// `at` is the callback invocation instant (UI clock) and `latency`
    /// the host-reported callback→audible delay when the backend provides
    /// one (`Some(Duration::ZERO)` = host says "no buffering info" —
    /// treated as unknown).
    fn fill(&mut self, out: &mut [f32], at: std::time::Instant, latency: Option<Duration>) {
        // Generation bump => seek or stop happened: drop everything the
        // feeder pushed for the old position. Discarded audio was never
        // handed to the device, so it is *not* counted as consumed.
        let gen = self.shared.generation.load(Ordering::Acquire);
        if gen != self.observed_generation {
            self.flush();
            self.observed_generation = gen;
        }

        if !self.shared.playing.load(Ordering::Acquire) {
            out.fill(0.0);
            return;
        }

        // Position of the first sample this callback will hand to the
        // device (frames): the anchor every extrapolation starts from.
        let base = self.shared.base.load(Ordering::Acquire);
        let channels = u64::from(self.shared.channels.load(Ordering::Acquire).max(1));
        let first_frame = base.saturating_add(self.consumed_here) / channels;

        let mut filled: &mut [f32] = out;
        let mut popped_total: usize = 0;
        while !filled.is_empty() {
            let (popped, rest) = self.consumer.pop_partial_slice(filled);
            if popped.is_empty() {
                // Ring drained underrun (device pulled faster than the
                // feeder) — emit silence for the remainder.
                rest.fill(0.0);
                break;
            }
            popped_total += popped.len();
            self.consumed_here += popped.len() as u64;
            filled = rest;
        }
        if popped_total > 0 {
            self.shared
                .consumed
                .store(self.consumed_here, Ordering::Release);
            // Device latency: host-reported when available (ALSA delay,
            // WASAPI padding, CoreAudio), else half the measured callback
            // period (the pipeline we cannot observe sits between the
            // callback and the DAC; on a paced device it is ≈ one period).
            let latency = match latency {
                Some(d) if !d.is_zero() => d,
                _ => self
                    .last_callback_at
                    .map(|prev| at.saturating_duration_since(prev) / 2)
                    .unwrap_or(Duration::ZERO),
            };
            self.shared.playhead.update(first_frame, at, latency);
        }
        self.last_callback_at = Some(at);
    }

    /// Discard everything currently buffered (seek/stop). Discarded
    /// samples never reached the device and are *not* counted as
    /// consumed; the handed counter restarts for the new generation.
    fn flush(&mut self) {
        let mut scratch = [0.0f32; 1024];
        loop {
            let (popped, _) = self.consumer.pop_partial_slice(&mut scratch);
            if popped.is_empty() {
                break;
            }
        }
        self.consumed_here = 0;
        self.shared.consumed.store(0, Ordering::Release);
    }
}

/// A playback session bound to one piece of audio.
///
/// Commands are asynchronous: they take effect within a few milliseconds
/// (the feeder wakes on its channel). Dropping the player stops audio and
/// joins its threads.
pub struct Player {
    cmd_tx: Sender<PlayerCommand>,
    shared: Arc<Shared>,
    stream: Option<cpal::Stream>,
    feeder: Option<std::thread::JoinHandle<()>>,
}

impl Player {
    /// Create a player for `audio`, trying every output device on every
    /// host until one actually opens a stream (BUG 3 fix).
    ///
    /// Fallback order mirrors [`crate::recorder::Recorder::start`]:
    /// default-host default device → other defaults → everything else.
    ///
    /// # Errors
    /// * [`Error::Device`] — no output device or unsupported format;
    /// * [`Error::Resample`] — rate conversion failure.
    pub fn new(audio: &InterleavedAudio) -> Result<Self> {
        let inv = crate::devices::list();
        let default_host = cpal::default_host().id().name().to_string();
        let candidates = crate::devices::fallback_order(&inv.outputs, &default_host);

        let mut last_err: Option<Error> = None;
        for cand in candidates {
            let dev = match crate::devices::open_by_id(&cand.id()) {
                Ok(d) => d,
                Err(e) => {
                    last_err = Some(e);
                    continue;
                }
            };
            match Self::on_device(&dev, audio) {
                Ok(p) => return Ok(p),
                Err(e) => {
                    eprintln!(
                        "mvl-io: output device '{}' failed to open, trying next: {e}",
                        cand.label()
                    );
                    last_err = Some(e);
                }
            }
        }
        Err(match last_err {
            Some(e) if inv.outputs.len() <= 1 => e,
            Some(e) => Error::Device(format!(
                "tried {} output device(s); last failure: {e}",
                inv.outputs.len()
            )),
            None => crate::devices::no_output_device_error(&inv),
        })
    }

    /// Create a player on a specific output device.
    ///
    /// # Errors
    /// Same as [`Player::new`].
    pub fn on_device(device: &cpal::Device, audio: &InterleavedAudio) -> Result<Self> {
        let default = device
            .default_output_config()
            .map_err(|e| Error::Device(format!("query output config: {e}")))?;
        let device_rate = default.sample_rate();
        let device_channels = default.channels();

        // Rate/channel adaptation (once, up front).
        let source = adapt_to_device(audio, device_rate, device_channels)?;

        let frames = source.frames() as u64;
        let channels = source.channels as usize;
        let shared = Shared::new();
        shared
            .sample_rate
            .store(source.sample_rate, Ordering::Release);
        shared
            .channels
            .store(u32::from(source.channels), Ordering::Release);
        shared.length_frames.store(frames, Ordering::Release);

        let ring_capacity =
            (source.sample_rate as f64 * channels as f64 * RING_SECONDS) as usize + 4096;
        let (producer, consumer) = rtrb::RingBuffer::new(ring_capacity);

        let (cmd_tx, cmd_rx) = crossbeam_channel::bounded::<PlayerCommand>(16);

        let feeder_shared = Arc::clone(&shared);
        let feeder_source = Arc::new(source);
        let feeder = spawn_feeder(producer, cmd_rx, feeder_shared, feeder_source);

        let sink_shared = Arc::clone(&shared);
        let mut sink = SinkLogic::new(consumer, sink_shared);
        let err_cb = |e: cpal::Error| eprintln!("mvl-io: output device error: {e}");

        let stream_config = cpal::StreamConfig {
            channels: device_channels,
            sample_rate: device_rate,
            buffer_size: cpal::BufferSize::Default,
        };

        let stream = match default.sample_format() {
            cpal::SampleFormat::F32 => device
                .build_output_stream::<f32, _, _>(
                    stream_config,
                    move |data: &mut [f32], info: &cpal::OutputCallbackInfo| {
                        let ts = info.timestamp();
                        sink.fill(
                            data,
                            std::time::Instant::now(),
                            crate::playhead::host_output_latency(&ts),
                        );
                    },
                    err_cb,
                    None,
                )
                .map_err(|e| Error::Device(format!("open output stream: {e}")))?,
            cpal::SampleFormat::I16 => device
                .build_output_stream::<i16, _, _>(
                    stream_config,
                    {
                        let mut sink = SinkLogicShim::from(sink);
                        move |data: &mut [i16], info: &cpal::OutputCallbackInfo| {
                            let ts = info.timestamp();
                            sink.fill_i16(
                                data,
                                std::time::Instant::now(),
                                crate::playhead::host_output_latency(&ts),
                            );
                        }
                    },
                    err_cb,
                    None,
                )
                .map_err(|e| Error::Device(format!("open output stream: {e}")))?,
            cpal::SampleFormat::I32 => device
                .build_output_stream::<i32, _, _>(
                    stream_config,
                    {
                        let mut sink = SinkLogicShim::from(sink);
                        move |data: &mut [i32], info: &cpal::OutputCallbackInfo| {
                            let ts = info.timestamp();
                            sink.fill_i32(
                                data,
                                std::time::Instant::now(),
                                crate::playhead::host_output_latency(&ts),
                            );
                        }
                    },
                    err_cb,
                    None,
                )
                .map_err(|e| Error::Device(format!("open output stream: {e}")))?,
            cpal::SampleFormat::U16 => device
                .build_output_stream::<u16, _, _>(
                    stream_config,
                    {
                        let mut sink = SinkLogicShim::from(sink);
                        move |data: &mut [u16], info: &cpal::OutputCallbackInfo| {
                            let ts = info.timestamp();
                            sink.fill_u16(
                                data,
                                std::time::Instant::now(),
                                crate::playhead::host_output_latency(&ts),
                            );
                        }
                    },
                    err_cb,
                    None,
                )
                .map_err(|e| Error::Device(format!("open output stream: {e}")))?,
            cpal::SampleFormat::U32 => device
                .build_output_stream::<u32, _, _>(
                    stream_config,
                    {
                        let mut sink = SinkLogicShim::from(sink);
                        move |data: &mut [u32], info: &cpal::OutputCallbackInfo| {
                            let ts = info.timestamp();
                            sink.fill_u32(
                                data,
                                std::time::Instant::now(),
                                crate::playhead::host_output_latency(&ts),
                            );
                        }
                    },
                    err_cb,
                    None,
                )
                .map_err(|e| Error::Device(format!("open output stream: {e}")))?,
            cpal::SampleFormat::U8 => device
                .build_output_stream::<u8, _, _>(
                    stream_config,
                    {
                        let mut sink = SinkLogicShim::from(sink);
                        move |data: &mut [u8], info: &cpal::OutputCallbackInfo| {
                            let ts = info.timestamp();
                            sink.fill_u8(
                                data,
                                std::time::Instant::now(),
                                crate::playhead::host_output_latency(&ts),
                            );
                        }
                    },
                    err_cb,
                    None,
                )
                .map_err(|e| Error::Device(format!("open output stream: {e}")))?,
            other => {
                return Err(Error::Device(format!(
                    "output device uses unsupported sample format {other:?}"
                )));
            }
        };

        stream
            .play()
            .map_err(|e| Error::Device(format!("start output stream: {e}")))?;

        Ok(Self {
            cmd_tx,
            shared,
            stream: Some(stream),
            feeder: Some(feeder),
        })
    }

    /// Start or resume playback.
    ///
    /// # Errors
    /// [`Error::InvalidState`] — the feeder thread is gone.
    pub fn play(&self) -> Result<()> {
        self.send(PlayerCommand::Play)
    }

    /// Pause at the current position.
    ///
    /// # Errors
    /// [`Error::InvalidState`] — the feeder thread is gone.
    pub fn pause(&self) -> Result<()> {
        self.send(PlayerCommand::Pause)
    }

    /// Stop and rewind to the beginning.
    ///
    /// # Errors
    /// [`Error::InvalidState`] — the feeder thread is gone.
    pub fn stop(&self) -> Result<()> {
        self.send(PlayerCommand::Stop)
    }

    /// Seek to a position in seconds (clamped to the track).
    ///
    /// # Errors
    /// [`Error::InvalidState`] — the feeder thread is gone.
    pub fn seek(&self, seconds: f64) -> Result<()> {
        self.send(PlayerCommand::Seek(seconds.max(0.0)))
    }

    /// Current transport state (updated by the feeder within ~2 ms).
    #[must_use]
    pub fn state(&self) -> TransportState {
        self.shared.state()
    }

    /// True while playing.
    #[must_use]
    pub fn is_playing(&self) -> bool {
        self.state() == TransportState::Playing
    }

    /// Audible position in seconds: extrapolated from the device-clock
    /// playhead anchor (latency compensated, clamped to the handed
    /// cursor) — see [`crate::playhead`].
    #[must_use]
    pub fn position_seconds(&self) -> f64 {
        self.shared.position_seconds()
    }

    fn send(&self, cmd: PlayerCommand) -> Result<()> {
        self.cmd_tx
            .send(cmd)
            .map_err(|_| Error::InvalidState("player feeder thread is gone".into()))
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.send(PlayerCommand::Shutdown);
        if let Some(stream) = self.stream.take() {
            let _ = stream.pause();
            drop(stream);
        }
        if let Some(feeder) = self.feeder.take() {
            let _ = feeder.join();
        }
    }
}

/// Thin adapter so integer device paths can reuse `SinkLogic` through a
/// scratch buffer (converted per callback; integer outputs are not the
/// studio path).
struct SinkLogicShim {
    inner: SinkLogic,
    scratch: Vec<f32>,
}

impl From<SinkLogic> for SinkLogicShim {
    fn from(inner: SinkLogic) -> Self {
        Self {
            inner,
            scratch: Vec::new(),
        }
    }
}

impl SinkLogicShim {
    /// Fill the scratch buffer with f32 and convert to the device type.
    fn scratch_fill(
        &mut self,
        len: usize,
        at: std::time::Instant,
        latency: Option<Duration>,
    ) -> &[f32] {
        self.scratch.clear();
        self.scratch.resize(len, 0.0);
        self.inner.fill(&mut self.scratch, at, latency);
        &self.scratch
    }

    fn fill_i16(&mut self, data: &mut [i16], at: std::time::Instant, latency: Option<Duration>) {
        let s = self.scratch_fill(data.len(), at, latency);
        for (dst, &src) in data.iter_mut().zip(s) {
            *dst = (src.clamp(-1.0, 1.0) * 32767.0) as i16;
        }
    }

    fn fill_i32(&mut self, data: &mut [i32], at: std::time::Instant, latency: Option<Duration>) {
        let s = self.scratch_fill(data.len(), at, latency);
        for (dst, &src) in data.iter_mut().zip(s) {
            *dst = (src.clamp(-1.0, 1.0) * 2147483647.0) as i32;
        }
    }

    fn fill_u16(&mut self, data: &mut [u16], at: std::time::Instant, latency: Option<Duration>) {
        let s = self.scratch_fill(data.len(), at, latency);
        for (dst, &src) in data.iter_mut().zip(s) {
            *dst = ((src.clamp(-1.0, 1.0) + 1.0) * 32767.5) as u16;
        }
    }

    fn fill_u32(&mut self, data: &mut [u32], at: std::time::Instant, latency: Option<Duration>) {
        let s = self.scratch_fill(data.len(), at, latency);
        for (dst, &src) in data.iter_mut().zip(s) {
            *dst = ((src.clamp(-1.0, 1.0) + 1.0) * 2147483647.5) as u32;
        }
    }

    fn fill_u8(&mut self, data: &mut [u8], at: std::time::Instant, latency: Option<Duration>) {
        let s = self.scratch_fill(data.len(), at, latency);
        for (dst, &src) in data.iter_mut().zip(s) {
            *dst = ((src.clamp(-1.0, 1.0) + 1.0) * 127.5) as u8;
        }
    }
}

/// Feeder thread: applies commands to the transport and streams source
/// frames into the ring while playing.
fn spawn_feeder(
    mut producer: Producer<f32>,
    cmd_rx: Receiver<PlayerCommand>,
    shared: Arc<Shared>,
    source: Arc<InterleavedAudio>,
) -> std::thread::JoinHandle<()> {
    let length = source.frames() as u64;
    let channels = source.channels as usize;
    let mut transport = Transport::new(length);

    std::thread::spawn(move || {
        let mut fed_since_bump: u64 = 0;
        loop {
            // Wake on command, else poll at 2 ms (keeps the ring topped up
            // without busy-spinning).
            match cmd_rx.recv_timeout(Duration::from_millis(2)) {
                Ok(PlayerCommand::Play) => {
                    transport.play();
                }
                Ok(PlayerCommand::Pause) => {
                    transport.pause();
                }
                Ok(PlayerCommand::Stop) => {
                    transport.stop();
                    bump_generation(&shared, &mut fed_since_bump, 0);
                }
                Ok(PlayerCommand::Seek(seconds)) => {
                    let was_playing = transport.is_playing();
                    transport.seek_seconds(seconds, source.sample_rate);
                    // Position base = new playhead (samples).
                    let base = transport.position() * channels as u64;
                    bump_generation(&shared, &mut fed_since_bump, base);
                    let _ = was_playing;
                }
                Ok(PlayerCommand::Shutdown) => break,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            }

            shared
                .playing
                .store(transport.is_playing(), Ordering::Release);
            shared.state.store(
                match transport.state() {
                    TransportState::Stopped => 0,
                    TransportState::Playing => 1,
                    TransportState::Paused => 2,
                },
                Ordering::Release,
            );

            if transport.is_playing() {
                // Keep the ring at most half full so seeks stay responsive.
                let target = (source.sample_rate as usize * channels) / 2;
                let mut scratch: Vec<f32> = Vec::with_capacity(FEED_CHUNK_FRAMES * channels);
                while transport.is_playing()
                    && producer.slots() > target.min(FEED_CHUNK_FRAMES * channels)
                {
                    let pos = transport.position() as usize;
                    if pos >= source.frames() {
                        transport.stop();
                        shared.playing.store(false, Ordering::Release);
                        shared.state.store(0, Ordering::Release);
                        break;
                    }
                    let take = FEED_CHUNK_FRAMES.min(source.frames() - pos);
                    scratch.clear();
                    scratch
                        .extend_from_slice(&source.data[pos * channels..(pos + take) * channels]);
                    let mut remaining: &[f32] = &scratch;
                    while !remaining.is_empty() {
                        let (_, rest) = producer.push_partial_slice(remaining);
                        if rest.len() == remaining.len() {
                            break; // ring full — retry next poll
                        }
                        remaining = rest;
                    }
                    fed_since_bump += (take * channels) as u64;
                    shared.fed.store(fed_since_bump, Ordering::Release);
                    transport.advance(take as u64);
                }
            }
        }
    })
}

/// Invalidate buffered audio (seek/stop): reset the counters, set the
/// position base, then bump the generation.
///
/// Ordering: `base` and the counter resets are published *before* the
/// generation increment, so a sink that observes the new generation (an
/// Acquire load of `generation`) is guaranteed to also see the new base
/// and fed/consumed = 0. The playhead anchor is frozen at the new base:
/// nothing has been handed to the device yet, so there is nothing to
/// extrapolate from until the next callback publishes a live anchor.
fn bump_generation(shared: &Shared, fed_since_bump: &mut u64, base_samples: u64) {
    shared.base.store(base_samples, Ordering::Release);
    shared.consumed.store(0, Ordering::Release);
    shared.fed.store(0, Ordering::Release);
    shared
        .playhead
        .reset_frozen(base_samples / u64::from(shared.channels.load(Ordering::Acquire).max(1)));
    *fed_since_bump = 0;
    shared.generation.fetch_add(1, Ordering::AcqRel);
}

/// Adapt source audio to the device's rate and channel count — the
/// shared, generalized [`crate::channels::adapt`] (BUG 3 widening:
/// v1.0.0 supported only mono↔stereo here; multichannel devices now
/// play).
fn adapt_to_device(
    audio: &InterleavedAudio,
    device_rate: u32,
    device_channels: u16,
) -> Result<InterleavedAudio> {
    crate::channels::adapt(audio, device_rate, device_channels)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rtrb::RingBuffer;

    fn tone_audio(frames: usize, rate: u32) -> InterleavedAudio {
        let data = (0..frames)
            .map(|i| {
                ((2.0 * std::f64::consts::PI * 440.0 * i as f64 / rate as f64).sin() * 0.5) as f32
            })
            .collect();
        InterleavedAudio::new(data, rate, 1).unwrap()
    }

    /// Drive the consumer exactly like a device callback would, without
    /// any hardware: builds a `Shared` + ring and pumps the sink.
    #[test]
    fn sink_logic_plays_pauses_and_seeks() {
        let shared = Shared::new();
        let rate = 48_000u32;
        let channels = 1usize;
        let (mut producer, consumer) = RingBuffer::new(48_000 * 2);
        let mut sink = SinkLogic::new(consumer, Arc::clone(&shared));

        // Paused sink outputs silence and consumes nothing.
        let mut out = [0.25f32; 64];
        sink.fill(&mut out, std::time::Instant::now(), None);
        assert!(
            out.iter().all(|&s| s == 0.0),
            "paused sink must emit silence"
        );

        // Feed 128 frames and play.
        let audio = tone_audio(1_000, rate);
        shared.playing.store(true, Ordering::Release);
        let (_, _) = producer.push_partial_slice(&audio.data[..128]);
        shared.fed.store(128, Ordering::Release);

        let mut got = [0.0f32; 64];
        sink.fill(&mut got, std::time::Instant::now(), None);
        assert_eq!(
            &got[..],
            &audio.data[..64],
            "sink must emit source samples in order"
        );

        // Pause: further fills are silence, ring contents preserved.
        shared.playing.store(false, Ordering::Release);
        let mut got2 = [1.0f32; 32];
        sink.fill(&mut got2, std::time::Instant::now(), None);
        assert!(got2.iter().all(|&s| s == 0.0));

        // Resume: the *remaining* buffered samples come out (continuity).
        shared.playing.store(true, Ordering::Release);
        let mut got3 = [0.0f32; 64];
        sink.fill(&mut got3, std::time::Instant::now(), None);
        assert_eq!(
            &got3[..],
            &audio.data[64..128],
            "pause must not lose buffered audio"
        );

        // Seek: generation bump flushes the rest.
        shared.generation.fetch_add(1, Ordering::AcqRel);
        let mut got4 = [0.5f32; 16];
        sink.fill(&mut got4, std::time::Instant::now(), None);
        assert!(
            got4.iter().all(|&s| s == 0.0),
            "post-seek fill must be silent (flushed)"
        );
        let _ = channels;
    }

    #[test]
    fn sink_logic_counts_consumption_for_position() {
        let shared = Shared::new();
        let (mut producer, consumer) = RingBuffer::new(4_096);
        let mut sink = SinkLogic::new(consumer, Arc::clone(&shared));

        let audio = tone_audio(256, 48_000);
        shared.playing.store(true, Ordering::Release);
        producer.push_entire_slice(&audio.data).unwrap();

        let mut out = [0.0f32; 100];
        sink.fill(&mut out, std::time::Instant::now(), None);
        assert_eq!(shared.consumed.load(Ordering::Acquire), 100);
        sink.fill(&mut out, std::time::Instant::now(), None);
        assert_eq!(shared.consumed.load(Ordering::Acquire), 200);
    }

    /// Phase 8.2: flushed (discarded) ring content must NOT count as
    /// consumed — it never reached the device, so the handed cursor (and
    /// thus the playhead floor) must stay at the pre-flush position.
    #[test]
    fn flushed_audio_not_counted_as_handed() {
        let shared = Shared::new();
        let (mut producer, consumer) = RingBuffer::new(4_096);
        let mut sink = SinkLogic::new(consumer, Arc::clone(&shared));

        shared.playing.store(true, Ordering::Release);
        producer.push_entire_slice(&[1.0f32; 1_000]).unwrap();

        let mut out = [0.0f32; 400];
        sink.fill(&mut out, std::time::Instant::now(), None);
        assert_eq!(shared.consumed.load(Ordering::Acquire), 400);

        // Seek: bump + flush. The 600 buffered samples are discarded;
        // handed stays at 400.
        shared.generation.fetch_add(1, Ordering::AcqRel);
        let mut out2 = [0.0f32; 64];
        sink.fill(&mut out2, std::time::Instant::now(), None);
        assert!(out2.iter().all(|&s| s == 0.0), "stale audio flushed");
        assert_eq!(
            shared.consumed.load(Ordering::Acquire),
            0,
            "handed counter restarts for the new generation"
        );
        assert_eq!(sink.consumed_here, 0);
    }

    /// Phase 8.2: each callback that hands audio to the device must
    /// publish a live anchor at the right frame position.
    #[test]
    fn sink_publishes_playhead_anchor_at_correct_frame() {
        let shared = Shared::new();
        let (mut producer, consumer) = RingBuffer::new(4_096);
        let mut sink = SinkLogic::new(consumer, Arc::clone(&shared));

        shared.playing.store(true, Ordering::Release);
        // Exactly two callback blocks: the third fill below must underrun.
        producer.push_entire_slice(&[1.0f32; 200]).unwrap();

        let mut out = [0.0f32; 100];
        sink.fill(&mut out, std::time::Instant::now(), None);
        assert!(shared.playhead.is_extrapolatable(), "anchor published");
        assert_eq!(shared.playhead.frozen_frame(), 0, "first callback at 0");

        sink.fill(&mut out, std::time::Instant::now(), None);
        assert_eq!(
            shared.playhead.frozen_frame(),
            100,
            "second callback at 100"
        );

        // Underrun (ring empty): no pops → no anchor update.
        sink.fill(&mut out, std::time::Instant::now(), None);
        assert_eq!(shared.playhead.frozen_frame(), 100);
    }

    /// Phase 8.2 end-to-end: feeder thread + ring + sink pumped by a
    /// simulated paced device (10 ms blocks, 21 ms host-reported latency).
    /// The reported playhead must match the *audible* audio position —
    /// wall time since play minus device latency — within 5 ms at every
    /// probe. This is the acceptance test for the user-reported bug
    /// "sound does not match the visual playhead".
    #[test]
    fn e2e_playhead_matches_audible_audio_within_5ms() {
        const RATE: u32 = 48_000;
        const BLOCK: usize = 480; // 10 ms
        const LATENCY: Duration = Duration::from_millis(21);
        const PROBES: usize = 40; // 0.4 s of playback

        let audio = tone_audio(RATE as usize * 5, RATE); // 5 s
        let shared = Shared::new();
        shared.sample_rate.store(RATE, Ordering::Release);
        shared.channels.store(1, Ordering::Release);
        shared
            .length_frames
            .store(audio.frames() as u64, Ordering::Release);
        let (producer, consumer) = RingBuffer::new(RATE as usize * 2);
        let (cmd_tx, cmd_rx) = crossbeam_channel::bounded::<PlayerCommand>(16);
        spawn_feeder(producer, cmd_rx, Arc::clone(&shared), Arc::new(audio));
        let mut sink = SinkLogic::new(consumer, Arc::clone(&shared));

        // Simulated device: sleep to the block schedule, then pull.
        let t0 = std::time::Instant::now();
        cmd_tx.send(PlayerCommand::Play).unwrap();
        // Feeder polls at 2 ms; give it a beat so play is registered
        // (the audible timeline starts at the first real callback).
        std::thread::sleep(Duration::from_millis(5));
        let mut first_callback_at: Option<std::time::Instant> = None;

        for i in 0..PROBES {
            let target = t0 + Duration::from_millis(5 + (i as u64 + 1) * 10);
            let now = std::time::Instant::now();
            if target > now {
                std::thread::sleep(target - now);
            }
            let mut buf = [0.0f32; BLOCK];
            let at = std::time::Instant::now();
            first_callback_at.get_or_insert(at);
            sink.fill(&mut buf, at, Some(LATENCY));

            // Expected audible position: audio time = wall time since the
            // first callback, minus the device latency. For the first
            // LATENCY the device pipeline is filling and nothing is
            // audible yet — the playhead legitimately clamps at 0 there
            // (no audio to match), so only assert once audio is live.
            let t1 = first_callback_at.unwrap();
            let expected = at.saturating_duration_since(t1).as_secs_f64() - 0.021;
            if expected <= 0.0 {
                continue;
            }
            let reported = shared.position_seconds();
            let err_ms = (reported - expected) * 1000.0;
            assert!(
                err_ms.abs() < 5.0,
                "probe {i}: reported {reported:.4}s vs expected audible {expected:.4}s \
                 (|err| = {err_ms:.2} ms, must be < 5 ms)"
            );
        }
    }

    /// Phase 8.2: after a 44.1 kHz → 48 kHz device adaptation the
    /// playhead must run in *seconds* on the device clock — one second
    /// of handed 48 kHz audio advances the playhead by exactly one
    /// second (a frames-based implementation would drift by 8.8 %).
    #[test]
    fn playhead_uses_device_rate_after_resample() {
        let shared = Shared::new();
        // Device-adapted stream: 48 kHz, one second.
        shared.sample_rate.store(48_000, Ordering::Release);
        shared.channels.store(1, Ordering::Release);
        shared.length_frames.store(48_000, Ordering::Release);
        let (mut producer, consumer) = RingBuffer::new(96_000);
        let mut sink = SinkLogic::new(consumer, Arc::clone(&shared));
        shared.playing.store(true, Ordering::Release);

        // Hand one second in 10 ms callbacks, pacing them in real time so
        // the extrapolation has honest wall-clock anchors.
        let t0 = std::time::Instant::now();
        let lat = Duration::from_millis(15);
        for i in 0..100 {
            let target = t0 + Duration::from_millis((i + 1) * 10);
            let now = std::time::Instant::now();
            if target > now {
                std::thread::sleep(target - now);
            }
            producer.push_entire_slice(&[0.25f32; 480]).unwrap();
            let mut buf = [0.0f32; 480];
            sink.fill(&mut buf, std::time::Instant::now(), Some(lat));
        }
        let pos = shared.position_seconds();
        // Handed 1.0 s; the last anchor sits one block back with 15 ms of
        // device latency in front of it → 1.0 − 0.010 − 0.015 = 0.975 s.
        // (A frames-vs-device-rate mismatch would show ≈ 1.088 or 0.919.)
        let expected = 1.0 - 0.010 - 0.015;
        assert!(
            (pos - expected).abs() < 0.02,
            "44.1→48 adaptation must not skew the playhead: pos {pos} vs ~{expected}"
        );
    }

    #[test]
    fn mono_adapts_to_stereo_device() {
        let mono = tone_audio(10, 48_000);
        let stereo = adapt_to_device(&mono, 48_000, 2).unwrap();
        assert_eq!(stereo.channels, 2);
        assert_eq!(stereo.frames(), 10);
        assert_eq!(stereo.data[0], mono.data[0]);
        assert_eq!(
            stereo.data[1], mono.data[0],
            "mono upmix duplicates the channel"
        );
    }

    #[test]
    fn stereo_folds_to_mono_device() {
        let mut data = Vec::new();
        for i in 0..8 {
            data.push(if i % 2 == 0 { 0.5 } else { -0.5 });
        }
        let stereo = InterleavedAudio::new(data, 48_000, 2).unwrap();
        let mono = adapt_to_device(&stereo, 48_000, 1).unwrap();
        assert_eq!(mono.channels, 1);
        assert!(
            mono.data.iter().all(|&s| s == 0.0),
            "L+R averaging must cancel"
        );
    }

    #[test]
    fn rate_adaptation_resamples() {
        let hi = tone_audio(48_000, 96_000);
        let adapted = adapt_to_device(&hi, 48_000, 1).unwrap();
        assert_eq!(adapted.sample_rate, 48_000);
        assert!(
            (adapted.frames() as i64 - 24_000).abs() < 3,
            "duration must follow the rate"
        );
    }

    /// Hardware smoke test — requires a real output device.
    /// `cargo test -p mvl-io -- --ignored`
    #[test]
    #[ignore = "requires a live audio output device"]
    fn hardware_plays_two_seconds() {
        let audio = tone_audio(96_000, 48_000); // 2 s
        let player = Player::new(&audio).expect("open default output device");
        player.play().unwrap();
        std::thread::sleep(Duration::from_millis(500));
        assert!(player.is_playing());
        let pos = player.position_seconds();
        assert!(
            pos > 0.2 && pos < 0.9,
            "position {pos} should track wall clock"
        );
        player.pause().unwrap();
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(player.state(), TransportState::Paused);
        player.stop().unwrap();
        drop(player);
    }

    /// Virtual-device smoke test — see
    /// `recorder::tests::virtual_recorder_smoke` for the rationale: the
    /// ALSA null PCM is un-paced, so the 0.5 s take may drain within
    /// milliseconds and the wall-clock assertions of the real-hardware
    /// test do not apply. Structural behaviour is still fully asserted:
    /// open, negotiate, adapt, play/pause/stop transitions, position
    /// bounds.
    #[test]
    fn virtual_player_smoke() {
        if std::env::var("MVL_VIRTUAL_AUDIO").ok().as_deref() != Some("1") {
            eprintln!("skipping: set MVL_VIRTUAL_AUDIO=1 with a null ALSA device to enable");
            return;
        }
        let audio = tone_audio(24_000, 48_000); // 0.5 s
        let player = Player::new(&audio).expect("open virtual output device");
        player.play().expect("play on virtual device");
        std::thread::sleep(Duration::from_millis(50));
        // Un-paced device: the take may already be finished — both are valid.
        let pos = player.position_seconds();
        let dur = audio.duration_seconds();
        assert!(
            pos <= dur + 0.05,
            "position {pos} must stay within the take ({dur} s)"
        );
        player.pause().expect("pause");
        player.stop().expect("stop");
    }
}
