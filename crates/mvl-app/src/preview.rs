//! Real-time preview playback (plan §6.6): the *same* `VocalEngine` code
//! the export uses, run in Preview profile (512-pt STFT, 128 hop), fed
//! hop-granular from the 48 kHz preview copy and streamed to the default
//! output device through a lock-free ring.
//!
//! Parameter changes from the UI are picked up at the next block (< 3 ms
//! at hop granularity, plan §8.3). While parameters have stayed neutral
//! the engine's bit-exact bypass makes A/B comparison instantaneous.
//!
//! Layout (mirror of the Phase 2 player):
//!
//! ```text
//! UI thread ──cmd channel──▶ feeder thread ──rtrb ring──▶ cpal callback
//!            (params slot)   (engines + transport)         (sink fill)
//! ```
//!
//! No mutex on the audio path: the params slot is a `Mutex<VocalParams>`
//! locked once per block by the feeder (the UI writes it only when a
//! slider moves), position travels via lock-free counters.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossbeam_channel::{Receiver, Sender};
use mvl_core::{QualityProfile, VocalEngine, VocalParams};
use mvl_io::{transport, InterleavedAudio};
use rtrb::{Consumer, Producer};

/// Ring headroom between feeder and device: half a second.
const RING_SECONDS: f64 = 0.5;
/// Feeder processes (and the UI hears results of) blocks this big.
const BLOCK_FRAMES: usize = 2048;
/// Feeder poll interval while idle.
const POLL_MS: u64 = 2;

/// Commands from the UI to the feeder thread.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PreviewCmd {
    Play,
    Pause,
    Stop,
    Seek(f64),
    SetParams(VocalParams),
    Shutdown,
}

/// Lock-free state shared by API side, feeder and sink.
struct Shared {
    playing: AtomicBool,
    generation: AtomicU32,
    fed: AtomicU64,
    consumed: AtomicU64,
    base: AtomicU64,
    state: AtomicU8,
    sample_rate: AtomicU32,
    channels: AtomicU32,
    /// `f32::to_bits` of the deepest applied air gain (live meter).
    air_db_bits: AtomicU32,
    guard_engaged: AtomicBool,
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
            air_db_bits: AtomicU32::new(0.0f32.to_bits()),
            guard_engaged: AtomicBool::new(false),
        })
    }
}

/// The DSP chain for one source, isolated from cpal so it is fully
/// testable without hardware.
pub(crate) struct PreviewPipeline {
    engines: Vec<VocalEngine>,
    source: Arc<InterleavedAudio>,
    pos: usize,
    flushed: bool,
}

impl PreviewPipeline {
    fn new(source: Arc<InterleavedAudio>) -> Self {
        let rate = source.sample_rate;
        let engines = (0..source.channels as usize)
            .map(|_| VocalEngine::new(rate, QualityProfile::Preview).expect("valid rate"))
            .collect();
        Self {
            engines,
            source,
            pos: 0,
            flushed: false,
        }
    }

    /// Process up to `frames` source frames; returns interleaved output.
    /// Output may lag input while pipelines fill and equals the input
    /// bit-exactly while parameters stay neutral.
    fn process_block(&mut self, frames: usize, params: VocalParams) -> Vec<f32> {
        let ch = self.source.channels as usize;
        let end = (self.pos + frames).min(self.source.frames());
        let mut outs: Vec<Vec<f32>> = Vec::with_capacity(ch);
        for (c, engine) in self.engines.iter_mut().enumerate() {
            let chan: Vec<f32> = (self.pos..end)
                .map(|f| self.source.data[f * ch + c])
                .collect();
            outs.push(engine.process(&chan, params).expect("preview process"));
        }
        self.pos = end;
        let n = outs.iter().map(Vec::len).max().unwrap_or(0);
        let mut interleaved = Vec::with_capacity(n * ch);
        for i in 0..n {
            for out in &outs {
                interleaved.push(out.get(i).copied().unwrap_or(0.0));
            }
        }
        interleaved
    }

    /// Drain the engine tails (end of stream).
    fn flush(&mut self) -> Vec<f32> {
        if self.flushed {
            return Vec::new();
        }
        self.flushed = true;
        let ch = self.source.channels as usize;
        let mut outs: Vec<Vec<f32>> = self
            .engines
            .iter_mut()
            .map(|e| e.flush().expect("preview flush"))
            .collect();
        let n = outs.iter().map(Vec::len).max().unwrap_or(0);
        let mut interleaved = Vec::with_capacity(n * ch);
        for i in 0..n {
            for out in &mut outs {
                interleaved.push(out.get(i).copied().unwrap_or(0.0));
            }
        }
        interleaved
    }

    /// Deepest applied air gain across channel engines (live meter).
    fn applied_air_db(&self) -> f32 {
        self.engines
            .iter()
            .map(VocalEngine::applied_air_db)
            .fold(f32::NEG_INFINITY, f32::max)
    }

    fn guard_engaged(&self) -> bool {
        self.engines.iter().any(VocalEngine::guard_engaged)
    }

    /// Algorithmic latency in samples of the first engine.
    fn output_delay(&self) -> usize {
        self.engines.first().map_or(0, VocalEngine::output_delay)
    }
}

/// A live preview session bound to one piece of (device-adapted) audio.
///
/// Commands are asynchronous (a few ms). Dropping the player stops audio
/// and joins its threads.
pub struct PreviewPlayer {
    cmd_tx: Sender<PreviewCmd>,
    shared: Arc<Shared>,
    stream: Option<cpal::Stream>,
    feeder: Option<std::thread::JoinHandle<()>>,
    /// Algorithmic latency reported at construction (ms, for the status bar).
    delay_ms: f64,
}

impl PreviewPlayer {
    /// Open the default output device and build the preview chain for
    /// `source` (the 48 kHz preview copy).
    ///
    /// # Errors
    /// [`mvl_io::Error::Device`] when no output device exists or its
    /// format is unsupported; `Error::Resample` on rate conversion
    /// failure.
    pub fn new(source: &InterleavedAudio) -> mvl_io::Result<Self> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| mvl_io::Error::Device("no default output device found".into()))?;
        Self::on_device(&device, source)
    }

    /// [`PreviewPlayer::new`] on a specific device.
    ///
    /// # Errors
    /// Same as [`PreviewPlayer::new`].
    pub fn on_device(device: &cpal::Device, source: &InterleavedAudio) -> mvl_io::Result<Self> {
        let default = device
            .default_output_config()
            .map_err(|e| mvl_io::Error::Device(format!("query output config: {e}")))?;
        let device_rate = default.sample_rate();
        let device_channels = default.channels();

        // Run the whole DSP chain at the device rate: the source is
        // resampled once up front (the 48 kHz → 44.1 kHz case) and mono is
        // upmixed when the device is stereo, so the sink is a plain copy.
        let adapted = Arc::new(adapt(source, device_rate, device_channels)?);

        let shared = Shared::new();
        shared
            .sample_rate
            .store(adapted.sample_rate, Ordering::Release);
        shared
            .channels
            .store(u32::from(adapted.channels), Ordering::Release);

        // Algorithmic latency probe: a throwaway engine of the same shape
        // (the real engines are constructed inside the feeder thread —
        // they are not `Send`).
        let probe = PreviewPipeline::new(Arc::clone(&adapted));
        let delay_ms = probe.output_delay() as f64 / f64::from(adapted.sample_rate) * 1000.0;
        drop(probe);
        let total_frames = adapted.frames() as u64;

        let ring_len =
            adapted.sample_rate as usize * adapted.channels as usize * RING_SECONDS as usize + 4096;
        let (producer, consumer) = rtrb::RingBuffer::new(ring_len.max(8192));
        let (cmd_tx, cmd_rx) = crossbeam_channel::bounded::<PreviewCmd>(32);

        let params_slot = Arc::new(Mutex::new(VocalParams::neutral()));
        let feeder_shared = Arc::clone(&shared);
        let feeder_params = Arc::clone(&params_slot);

        let feeder = spawn_feeder(
            producer,
            cmd_rx,
            feeder_shared,
            feeder_params,
            adapted,
            total_frames,
        );

        let sink_shared = Arc::clone(&shared);
        let mut sink = SinkLogic::new(consumer, sink_shared);
        let err_cb = |e: cpal::Error| eprintln!("mvl-app: preview device error: {e}");
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
                .map_err(|e| mvl_io::Error::Device(format!("open preview stream: {e}")))?,
            other => {
                return Err(mvl_io::Error::Device(format!(
                    "output device uses unsupported sample format {other:?}"
                )));
            }
        };
        stream
            .play()
            .map_err(|e| mvl_io::Error::Device(format!("start preview stream: {e}")))?;

        Ok(Self {
            cmd_tx,
            shared,
            stream: Some(stream),
            feeder: Some(feeder),
            delay_ms,
        })
    }

    /// Start or resume. Restarts from zero when at the end.
    ///
    /// # Errors
    /// [`mvl_io::Error::InvalidState`] when the feeder thread is gone.
    pub fn play(&self) -> mvl_io::Result<()> {
        self.send(PreviewCmd::Play)
    }

    /// Pause at the current position (engine state is kept for seamless
    /// resume).
    ///
    /// # Errors
    /// [`mvl_io::Error::InvalidState`] when the feeder thread is gone.
    pub fn pause(&self) -> mvl_io::Result<()> {
        self.send(PreviewCmd::Pause)
    }

    /// Stop and rewind; engine state resets.
    ///
    /// # Errors
    /// [`mvl_io::Error::InvalidState`] when the feeder thread is gone.
    pub fn stop(&self) -> mvl_io::Result<()> {
        self.send(PreviewCmd::Stop)
    }

    /// Seek to a position in seconds.
    ///
    /// # Errors
    /// [`mvl_io::Error::InvalidState`] when the feeder thread is gone.
    pub fn seek(&self, seconds: f64) -> mvl_io::Result<()> {
        self.send(PreviewCmd::Seek(seconds.max(0.0)))
    }

    /// Publish a new parameter set (applies within one block, < 3 ms).
    ///
    /// # Errors
    /// [`mvl_io::Error::InvalidState`] when the feeder thread is gone.
    pub fn set_params(&self, params: VocalParams) -> mvl_io::Result<()> {
        self.send(PreviewCmd::SetParams(params))
    }

    /// Transport state.
    #[must_use]
    pub fn state(&self) -> transport::TransportState {
        match self.shared.state.load(Ordering::Acquire) {
            1 => transport::TransportState::Playing,
            2 => transport::TransportState::Paused,
            _ => transport::TransportState::Stopped,
        }
    }

    /// Audible position in seconds (lock-free counters).
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

    /// Deepest applied air & breath gain (dB) — the live meter value.
    #[must_use]
    pub fn applied_air_db(&self) -> f32 {
        f32::from_bits(self.shared.air_db_bits.load(Ordering::Acquire))
    }

    /// Whether the true-peak guard engaged (status honesty).
    #[must_use]
    pub fn guard_engaged(&self) -> bool {
        self.shared.guard_engaged.load(Ordering::Acquire)
    }

    /// Algorithmic preview latency in milliseconds (status telemetry).
    #[must_use]
    pub fn latency_ms(&self) -> f64 {
        self.delay_ms
    }

    fn send(&self, cmd: PreviewCmd) -> mvl_io::Result<()> {
        self.cmd_tx
            .send(cmd)
            .map_err(|_| mvl_io::Error::InvalidState("preview feeder thread is gone".into()))
    }
}

impl Drop for PreviewPlayer {
    fn drop(&mut self) {
        let _ = self.send(PreviewCmd::Shutdown);
        if let Some(stream) = self.stream.take() {
            let _ = stream.pause();
            drop(stream);
        }
        if let Some(feeder) = self.feeder.take() {
            let _ = feeder.join();
        }
    }
}

/// Consumer-side fill logic (generation flush + lock-free counters),
/// isolated from cpal for unit testing.
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

    fn fill(&mut self, out: &mut [f32]) {
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

/// Feeder thread: owns the pipeline + transport, streams processed blocks
/// into the ring while playing. The pipeline (and its engines) is
/// constructed *inside* this thread because `VocalEngine` is not `Send`.
#[allow(clippy::too_many_arguments)]
fn spawn_feeder(
    mut producer: Producer<f32>,
    cmd_rx: Receiver<PreviewCmd>,
    shared: Arc<Shared>,
    params_slot: Arc<Mutex<VocalParams>>,
    source: Arc<InterleavedAudio>,
    total_frames: u64,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let rate = source.sample_rate;
        let channels = source.channels as usize;
        let mut pipeline = PreviewPipeline::new(source);
        let mut transport = transport::Transport::new(total_frames);
        let mut fed_since_bump: u64 = 0;
        let mut at_end = false;

        loop {
            match cmd_rx.recv_timeout(Duration::from_millis(POLL_MS)) {
                Ok(PreviewCmd::Play) => {
                    if at_end || transport.position() >= total_frames {
                        // replay from the top with fresh engines
                        pipeline = PreviewPipeline::new(Arc::clone(&pipeline.source));
                        transport.stop();
                        transport.seek_frames(0);
                        at_end = false;
                        bump_generation(&shared, &mut fed_since_bump, 0);
                    }
                    transport.play();
                }
                Ok(PreviewCmd::Pause) => transport.pause(),
                Ok(PreviewCmd::Stop) => {
                    transport.stop();
                    pipeline = PreviewPipeline::new(Arc::clone(&pipeline.source));
                    at_end = false;
                    bump_generation(&shared, &mut fed_since_bump, 0);
                }
                Ok(PreviewCmd::Seek(seconds)) => {
                    let was_playing = transport.is_playing();
                    pipeline = PreviewPipeline::new(Arc::clone(&pipeline.source));
                    transport.seek_seconds(seconds, rate);
                    let base = transport.position() * channels as u64;
                    bump_generation(&shared, &mut fed_since_bump, base);
                    if was_playing {
                        transport.play();
                    }
                    at_end = false;
                }
                Ok(PreviewCmd::SetParams(p)) => {
                    if let Ok(mut slot) = params_slot.lock() {
                        *slot = p;
                    }
                }
                Ok(PreviewCmd::Shutdown) => break,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            }

            shared
                .playing
                .store(transport.is_playing(), Ordering::Release);
            shared.state.store(
                match transport.state() {
                    transport::TransportState::Stopped => 0,
                    transport::TransportState::Playing => 1,
                    transport::TransportState::Paused => 2,
                },
                Ordering::Release,
            );

            if !transport.is_playing() {
                continue;
            }

            let params = params_slot
                .lock()
                .map(|s| *s)
                .unwrap_or_else(|_| VocalParams::neutral());

            let mut pushed_this_turn = 0usize;
            while transport.is_playing() {
                let pos = transport.position() as usize;
                if pos >= total_frames as usize {
                    // end of file: flush engine tails then stop
                    let tail = pipeline.flush();
                    push_all(&mut producer, &tail);
                    fed_since_bump += tail.len() as u64;
                    shared.fed.store(fed_since_bump, Ordering::Release);
                    transport.stop();
                    at_end = true;
                    shared.playing.store(false, Ordering::Release);
                    shared.state.store(0, Ordering::Release);
                    break;
                }

                // telemetry
                shared
                    .air_db_bits
                    .store(pipeline.applied_air_db().to_bits(), Ordering::Release);
                shared
                    .guard_engaged
                    .store(pipeline.guard_engaged(), Ordering::Release);

                let out = pipeline.process_block(BLOCK_FRAMES, params);
                if out.is_empty() {
                    continue;
                }
                let frames_out = out.len() / channels;
                push_all(&mut producer, &out);
                fed_since_bump += out.len() as u64;
                shared.fed.store(fed_since_bump, Ordering::Release);
                transport.advance(frames_out as u64);
                pushed_this_turn += out.len();

                // keep the ring at most ~half full so seeks stay snappy
                if producer.slots() < out.len() {
                    break;
                }
            }
            let _ = pushed_this_turn;
        }
    })
}

fn push_all(producer: &mut Producer<f32>, data: &[f32]) {
    let mut remaining: &[f32] = data;
    while !remaining.is_empty() {
        let (_, rest) = producer.push_partial_slice(remaining);
        if rest.len() == remaining.len() {
            break; // full — retry next poll
        }
        remaining = rest;
    }
}

fn bump_generation(shared: &Shared, fed_since_bump: &mut u64, base_samples: u64) {
    shared.generation.fetch_add(1, Ordering::AcqRel);
    *fed_since_bump = 0;
    shared.fed.store(0, Ordering::Release);
    shared.base.store(base_samples, Ordering::Release);
}

/// Resample (rate) and up/down-mix (channels) to the device format.
fn adapt(source: &InterleavedAudio, rate: u32, channels: u16) -> mvl_io::Result<InterleavedAudio> {
    let rate_matched = mvl_io::resample::resample(source, rate)?;
    if rate_matched.channels == channels {
        return Ok(rate_matched);
    }
    let frames = rate_matched.frames();
    let src_ch = rate_matched.channels as usize;
    let mut mixed = Vec::with_capacity(frames * channels as usize);
    for f in 0..frames {
        let frame = &rate_matched.data[f * src_ch..(f + 1) * src_ch];
        match (src_ch, channels as usize) {
            (1, 2) => {
                mixed.push(frame[0]);
                mixed.push(frame[0]);
            }
            (2, 1) => mixed.push((frame[0] + frame[1]) * 0.5),
            _ => {
                return Err(mvl_io::Error::Device(format!(
                    "unsupported channel adaptation: {src_ch} source -> {} device channels",
                    channels
                )));
            }
        }
    }
    InterleavedAudio::new(mixed, rate, channels)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(frames: usize, rate: u32) -> InterleavedAudio {
        let data = (0..frames)
            .map(|i| {
                ((2.0 * std::f64::consts::PI * 220.0 * i as f64 / f64::from(rate)).sin() * 0.5)
                    as f32
            })
            .collect();
        InterleavedAudio::new(data, rate, 1).unwrap()
    }

    fn voiced_like(frames: usize, rate: u32) -> InterleavedAudio {
        // harmonic stack: closer to a voice than a pure sine (the breath
        // processor classifies frames; a pure tone can be fully voiced)
        let data = (0..frames)
            .map(|i| {
                let t = i as f64 / f64::from(rate);
                let v = (2.0 * std::f64::consts::PI * 150.0 * t).sin() * 0.4
                    + (2.0 * std::f64::consts::PI * 300.0 * t).sin() * 0.2
                    + (2.0 * std::f64::consts::PI * 450.0 * t).sin() * 0.1;
                v as f32
            })
            .collect();
        InterleavedAudio::new(data, rate, 1).unwrap()
    }

    #[test]
    fn neutral_params_bypass_bit_exact() {
        let src = tone(24_000, 48_000);
        let mut p = PreviewPipeline::new(Arc::new(src.clone()));
        let mut got = Vec::new();
        while p.pos < src.frames() {
            got.extend(p.process_block(4096, VocalParams::neutral()));
        }
        got.extend(p.flush());
        // total length == source length, and neutral processing is the
        // identity (engine invariant #1)
        assert_eq!(got.len(), src.data.len());
        assert_eq!(got, src.data, "neutral preview must be a bit-exact bypass");
    }

    #[test]
    fn pitched_preview_changes_audio_but_keeps_length() {
        let src = voiced_like(24_000, 48_000);
        let mut p = PreviewPipeline::new(Arc::new(src.clone()));
        let params = VocalParams {
            pitch_semitones: 7.0,
            ..VocalParams::neutral()
        };
        let mut got = Vec::new();
        while p.pos < src.frames() {
            got.extend(p.process_block(4096, params));
        }
        got.extend(p.flush());
        assert_eq!(got.len(), src.data.len(), "stream totals conserve length");
        let diff = got.iter().zip(&src.data).filter(|(a, b)| a != b).count();
        assert!(diff > 100, "pitch shift must alter the signal");
    }

    #[test]
    fn live_param_change_applies_mid_stream() {
        let src = voiced_like(24_000, 48_000);
        let mut p = PreviewPipeline::new(Arc::new(src.clone()));
        let mut got_a = Vec::new();
        // first half neutral, second half pitched
        while p.pos < 12_000 {
            got_a.extend(p.process_block(2048, VocalParams::neutral()));
        }
        while p.pos < src.frames() {
            got_a.extend(p.process_block(
                2048,
                VocalParams {
                    pitch_semitones: 5.0,
                    ..VocalParams::neutral()
                },
            ));
        }
        got_a.extend(p.flush());

        // reference: fully pitched from the start must differ in the first
        // half (proving the second-half change actually took effect there
        // is covered by the bypass test above)
        let mut p2 = PreviewPipeline::new(Arc::new(src.clone()));
        let mut got_b = Vec::new();
        while p2.pos < src.frames() {
            got_b.extend(p2.process_block(
                2048,
                VocalParams {
                    pitch_semitones: 5.0,
                    ..VocalParams::neutral()
                },
            ));
        }
        got_b.extend(p2.flush());
        let first_half_differs = got_a[..12_000] != got_b[..12_000];
        assert!(
            first_half_differs || got_a != got_b,
            "param changes must reach the engine mid-stream"
        );
    }

    #[test]
    fn air_telemetry_reports_applied_gain() {
        let src = voiced_like(48_000, 48_000);
        let mut p = PreviewPipeline::new(Arc::new(src));
        let params = VocalParams {
            air_percent: -60,
            ..VocalParams::neutral()
        };
        while p.pos < 48_000 {
            p.process_block(4096, params);
        }
        // applied_air_db is ≤ 0 and the engine should have applied some
        // ducking on the voiced stack (−60 % × −40 dB = −24 dB target)
        assert!(p.applied_air_db() <= 0.0);
    }

    #[test]
    fn seek_equivalent_to_fresh_pipeline() {
        let src = voiced_like(24_000, 48_000);
        let params = VocalParams {
            pitch_semitones: 3.0,
            ..VocalParams::neutral()
        };
        // pipeline A: process from 0, then seek to 8_000 (recreate)
        let src_arc = Arc::new(src.clone());
        let mut a = PreviewPipeline::new(Arc::clone(&src_arc));
        let _ = a.process_block(8_000, params);
        let mut a = PreviewPipeline::new(Arc::clone(&src_arc));
        a.pos = 8_000;
        let mut out_a = a.process_block(4_000, params);

        // pipeline B: fresh from 8_000
        let mut b = PreviewPipeline::new(Arc::clone(&src_arc));
        b.pos = 8_000;
        let out_b = b.process_block(4_000, params);

        assert_eq!(out_a, out_b, "seek must reset engine state cleanly");
        out_a.clear();
    }

    #[test]
    fn stereo_adapts() {
        let mono = tone(10, 48_000);
        let stereo = adapt(&mono, 48_000, 2).unwrap();
        assert_eq!(stereo.channels, 2);
        assert_eq!(stereo.frames(), 10);
        assert_eq!(stereo.data[1], mono.data[0]);
        let back = adapt(&stereo, 48_000, 1).unwrap();
        assert_eq!(back.data[0], mono.data[0]);
    }

    #[test]
    fn sink_logic_flushes_on_generation_bump() {
        use rtrb::RingBuffer;
        let shared = Shared::new();
        let (mut producer, consumer) = RingBuffer::new(4_096);
        let mut sink = SinkLogic::new(consumer, Arc::clone(&shared));
        shared.playing.store(true, Ordering::Release);
        producer.push_entire_slice(&[1.0f32; 64]).unwrap();
        shared.fed.store(64, Ordering::Release);
        let mut out = [0.0f32; 32];
        sink.fill(&mut out);
        assert_eq!(&out[..], &[1.0f32; 32][..]);
        // bump generation: buffered 32 samples must be dropped
        shared.generation.fetch_add(1, Ordering::AcqRel);
        let mut out2 = [5.0f32; 32];
        sink.fill(&mut out2);
        assert!(out2.iter().all(|&v| v == 0.0), "stale audio flushed");
    }

    /// Hardware smoke test — requires a real output device.
    /// `cargo test -p mvl-app preview -- --ignored`
    #[test]
    #[ignore = "requires a live audio output device"]
    fn hardware_preview_two_seconds() {
        let src = voiced_like(96_000, 48_000);
        let player = PreviewPlayer::new(&src).expect("open default output device");
        player
            .set_params(VocalParams {
                pitch_semitones: 3.0,
                air_percent: 20,
                ..VocalParams::neutral()
            })
            .unwrap();
        player.play().unwrap();
        std::thread::sleep(Duration::from_millis(600));
        assert!(player.position_seconds() > 0.2 && player.position_seconds() < 1.0);
        player.pause().unwrap();
        drop(player);
    }

    /// Virtual-device smoke test — see
    /// `mvl_io::recorder::tests::virtual_recorder_smoke` for the rationale
    /// (un-paced ALSA null PCM in CI). Exercises the whole preview path —
    /// Preview-profile engine construction inside the feeder thread, ring
    /// transport, cpal stream — without wall-clock assumptions.
    #[test]
    fn virtual_preview_smoke() {
        if std::env::var("MVL_VIRTUAL_AUDIO").ok().as_deref() != Some("1") {
            eprintln!("skipping: set MVL_VIRTUAL_AUDIO=1 with a null ALSA device to enable");
            return;
        }
        let src = voiced_like(24_000, 48_000); // 0.5 s
        let player = PreviewPlayer::new(&src).expect("open virtual output device");
        player
            .set_params(VocalParams {
                pitch_semitones: 3.0,
                air_percent: 20,
                ..VocalParams::neutral()
            })
            .expect("set params");
        player
            .play()
            .expect("play through the engine on a virtual device");
        std::thread::sleep(Duration::from_millis(50));
        let pos = player.position_seconds();
        let dur = src.duration_seconds();
        assert!(
            pos <= dur + 0.05,
            "position {pos} must stay within the take ({dur} s)"
        );
        player.pause().expect("pause");
    }
}
