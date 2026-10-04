//! Audio playback via [cpal] with play / pause / stop / seek.
//!
//! Architecture (mirror of the recorder):
//!
//! * a **feeder thread** owns the [`Transport`] and the source audio,
//!   pushes interleaved `f32` into a lock-free ring while playing;
//! * the **cpal output callback** owns the consumer side through
//!   [`SinkLogic`], which fills each device buffer (silence when paused,
//!   data when playing, and flushes stale audio after seek/stop via a
//!   generation counter);
//! * commands travel over a crossbeam channel; position is reported from
//!   lock-free counters (`base + fed − consumed`), no mutexes anywhere on
//!   the audio path.
//!
//! If the output device runs at a different sample rate than the source,
//! the source is resampled once via [`crate::resample`] at construction
//! (the 192 kHz session → 48 kHz device case, plan §6.6). Channel-count
//! mismatches are handled by a minimal up/down-mix (mono↔stereo).

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
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
    /// Samples pushed by the feeder since the last generation bump.
    fed: AtomicU64,
    /// Samples consumed by the sink since the last generation bump.
    consumed: AtomicU64,
    /// `position_base` in samples (frames × channels) at last bump.
    base: AtomicU64,
    /// Mirrored `TransportState` for the API side (0/1/2).
    state: AtomicU8,
    /// Sample rate of the (possibly resampled) playback stream.
    sample_rate: AtomicU32,
    channels: AtomicU32,
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
        })
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
struct SinkLogic {
    consumer: Consumer<f32>,
    shared: Arc<Shared>,
    observed_generation: u32,
    consumed_here: u64,
}

impl SinkLogic {
    fn new(consumer: Consumer<f32>, shared: Arc<Shared>) -> Self {
        let observed_generation = shared.generation.load(Ordering::Acquire);
        Self {
            consumer,
            shared,
            observed_generation,
            consumed_here: 0,
        }
    }

    /// Fill `out` for one device callback: data while playing, silence
    /// otherwise; stale post-seek audio is flushed first.
    fn fill(&mut self, out: &mut [f32]) {
        // Generation bump => seek or stop happened: drop everything the
        // feeder pushed for the old position.
        let gen = self.shared.generation.load(Ordering::Acquire);
        if gen != self.observed_generation {
            self.flush();
            self.observed_generation = gen;
        }

        if !self.shared.playing.load(Ordering::Acquire) {
            out.fill(0.0);
            return;
        }

        let mut filled: &mut [f32] = out;
        while !filled.is_empty() {
            let (popped, rest) = self.consumer.pop_partial_slice(filled);
            if popped.is_empty() {
                // Ring drained underrun (device pulled faster than the
                // feeder) — emit silence for the remainder.
                rest.fill(0.0);
                break;
            }
            self.consumed_here += popped.len() as u64;
            filled = rest;
        }
        self.shared
            .consumed
            .store(self.consumed_here, Ordering::Release);
    }

    /// Discard everything currently buffered.
    fn flush(&mut self) {
        let mut scratch = [0.0f32; 1024];
        loop {
            let (popped, _) = self.consumer.pop_partial_slice(&mut scratch);
            if popped.is_empty() {
                break;
            }
            self.consumed_here += popped.len() as u64;
        }
        self.shared
            .consumed
            .store(self.consumed_here, Ordering::Release);
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
    /// Create a player for `audio`, opening the default output device.
    ///
    /// The source is resampled to the device rate (and mono↔stereo mixed)
    /// if needed, once, up front.
    ///
    /// # Errors
    /// [`Error::Device`] — no output device or unsupported format;
    /// [`Error::Resample`] — rate conversion failure.
    pub fn new(audio: &InterleavedAudio) -> Result<Self> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| Error::Device("no default output device found".into()))?;
        Self::on_device(&device, audio)
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
                    move |data: &mut [f32], _: &cpal::OutputCallbackInfo| sink.fill(data),
                    err_cb,
                    None,
                )
                .map_err(|e| Error::Device(format!("open output stream: {e}")))?,
            cpal::SampleFormat::I16 => device
                .build_output_stream::<i16, _, _>(
                    stream_config,
                    {
                        let mut sink = SinkLogicShim::from(sink);
                        move |data: &mut [i16], _: &cpal::OutputCallbackInfo| sink.fill_i16(data)
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

        let _ = frames;
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

    /// Audible position in seconds: frames pulled by the device, derived
    /// from the lock-free counters (`base + fed − consumed`).
    #[must_use]
    pub fn position_seconds(&self) -> f64 {
        let base = self.shared.base.load(Ordering::Acquire);
        let fed = self.shared.fed.load(Ordering::Acquire);
        let consumed = self.shared.consumed.load(Ordering::Acquire);
        let samples = base + fed.saturating_sub(consumed);
        let channels = u64::from(self.shared.channels.load(Ordering::Acquire).max(1));
        let rate = f64::from(self.shared.sample_rate.load(Ordering::Acquire).max(1));
        (samples / channels) as f64 / rate
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

/// Thin adapter so the i16 device path can reuse `SinkLogic` through a
/// scratch buffer (converted per callback; i16 outputs are not the
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
    fn fill_i16(&mut self, data: &mut [i16]) {
        self.scratch.clear();
        self.scratch.resize(data.len(), 0.0);
        self.inner.fill(&mut self.scratch);
        for (dst, &src) in data.iter_mut().zip(&self.scratch) {
            *dst = (src.clamp(-1.0, 1.0) * 32767.0) as i16;
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

/// Invalidate buffered audio (seek/stop): bump generation, reset counters,
/// set the position base.
fn bump_generation(shared: &Shared, fed_since_bump: &mut u64, base_samples: u64) {
    shared.generation.fetch_add(1, Ordering::AcqRel);
    *fed_since_bump = 0;
    shared.fed.store(0, Ordering::Release);
    shared.base.store(base_samples, Ordering::Release);
}

/// Resample (rate) and up/down-mix (channels) the source to what the
/// device wants. No-op when both already match.
fn adapt_to_device(
    audio: &InterleavedAudio,
    device_rate: u32,
    device_channels: u16,
) -> Result<InterleavedAudio> {
    let rate_matched = crate::resample::resample(audio, device_rate)?;
    if rate_matched.channels == device_channels {
        return Ok(rate_matched);
    }
    let frames = rate_matched.frames();
    let src_ch = rate_matched.channels as usize;
    let mut mixed = Vec::with_capacity(frames * device_channels as usize);
    for f in 0..frames {
        let frame = &rate_matched.data[f * src_ch..(f + 1) * src_ch];
        match (src_ch, device_channels) {
            (1, 2) => {
                mixed.push(frame[0]);
                mixed.push(frame[0]);
            }
            (2, 1) => mixed.push((frame[0] + frame[1]) * 0.5),
            _ => {
                return Err(Error::Device(format!(
                    "unsupported channel adaptation: {} source -> {} device channels",
                    src_ch, device_channels
                )));
            }
        }
    }
    InterleavedAudio::new(mixed, device_rate, device_channels)
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
        sink.fill(&mut out);
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
        sink.fill(&mut got);
        assert_eq!(
            &got[..],
            &audio.data[..64],
            "sink must emit source samples in order"
        );

        // Pause: further fills are silence, ring contents preserved.
        shared.playing.store(false, Ordering::Release);
        let mut got2 = [1.0f32; 32];
        sink.fill(&mut got2);
        assert!(got2.iter().all(|&s| s == 0.0));

        // Resume: the *remaining* buffered samples come out (continuity).
        shared.playing.store(true, Ordering::Release);
        let mut got3 = [0.0f32; 64];
        sink.fill(&mut got3);
        assert_eq!(
            &got3[..],
            &audio.data[64..128],
            "pause must not lose buffered audio"
        );

        // Seek: generation bump flushes the rest.
        shared.generation.fetch_add(1, Ordering::AcqRel);
        let mut got4 = [0.5f32; 16];
        sink.fill(&mut got4);
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
        sink.fill(&mut out);
        assert_eq!(shared.consumed.load(Ordering::Acquire), 100);
        sink.fill(&mut out);
        assert_eq!(shared.consumed.load(Ordering::Acquire), 200);
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
}
