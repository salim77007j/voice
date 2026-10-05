//! Playhead synchronization — device-clock anchored audible position.
//!
//! Phase 8.2: the playhead the UI shows must match **what the listener
//! hears**, not what the feeder has queued. Two independent errors made
//! the old playhead drift visibly ahead of the audio:
//!
//! 1. **Wrong cursor.** Position was derived as `base + fed − consumed`
//!    (in samples). `fed − consumed` is the *ring occupancy* — samples
//!    that are buffered but have never been handed to the device — so the
//!    reported position led the audible audio by the whole ring fill
//!    (hundreds of milliseconds), jittering up and down with every ring
//!    top-up.
//! 2. **No device-latency compensation.** Even `base + consumed` (the
//!    *handed* cursor) is ahead of what is audible: the OS/device holds
//!    another buffer of audio (≈ 10–50 ms) between the callback and the
//!    DAC.
//!
//! The fix anchors the playhead in the **audio callback** itself, on the
//! device's own clock:
//!
//! * each callback that pops audio records a triple — the frame position
//!   of the first popped sample, the host's predicted *playback* instant
//!   for that sample (`cpal::OutputStreamTimestamp::playback − callback`
//!   = measured device latency), and the callback's `Instant::now()` —
//!   into a seqlock-protected lock-free slot (4 atomic stores, no
//!   allocation, no lock: real-time safe);
//! * the UI thread extrapolates between callbacks:
//!   `audible(T) = first_frame + (T − callback − latency) · rate`, which
//!   is exact between callbacks (the device drains its buffer at a
//!   constant rate), re-anchors every callback (so device/system clock
//!   drift cannot accumulate), and naturally interpolates the playhead
//!   across UI ticks (30 fps display of a ~100–1000 Hz device clock).
//!
//! `StreamInstant` values are opaque host-clock readings and cannot be
//! compared across threads; pairing the callback's `Instant::now()` with
//! the host's *relative* `playback − callback` duration maps the anchor
//! into the process monotonic clock without losing the device-clock
//! semantics. Position is only ever reported for audio that has actually
//! been handed to the device (clamped to the handed cursor), so underruns
//! freeze the playhead instead of running it ahead of silence.

use std::sync::atomic::{AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Process-monotonic epoch: first caller pins it, every later reading is
/// nanoseconds since then (u64 covers ~584 years).
static EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

/// Nanoseconds since the process epoch (monotonic, never goes backwards).
///
/// The epoch is pinned by [`PlayheadAnchor::new`]; this function is only
/// called with instants captured afterwards (or with the epoch's own
/// reading), so the saturating fallback below is belt-and-braces.
#[inline]
fn instant_ns(t: Instant) -> u64 {
    let epoch = EPOCH.get_or_init(Instant::now);
    t.checked_duration_since(*epoch)
        .unwrap_or(Duration::ZERO)
        .as_nanos() as u64
}

/// Nanoseconds of `now` on the same clock as [`instant_ns`].
#[inline]
fn now_ns() -> u64 {
    instant_ns(Instant::now())
}

/// Latency sentinel meaning "anchor frozen at `first_frame`, not
/// extrapolatable" (used between a seek/stop and the next callback).
const LATENCY_INVALID: i64 = -1;

/// Seqlock-protected playhead anchor written by the audio callback and
/// read by the UI thread.
///
/// Writer side ([`PlayheadAnchor::update`]) is real-time safe: four
/// atomic stores and one `Instant::now()`. Single writer (cpal invokes
/// the data callback from one thread), lock-free seqlock reader with a
/// bounded retry so a UI read can never spin against a hot writer.
#[derive(Debug)]
pub struct PlayheadAnchor {
    /// Seqlock: even = stable, odd = write in progress.
    seq: AtomicU32,
    /// Frame position (mono-frame units) of the first sample of the block
    /// described by this anchor.
    first_frame: AtomicU64,
    /// `Instant::now()` taken in the callback, ns since process epoch.
    at_ns: AtomicU64,
    /// Measured device latency in ns: time from callback invocation until
    /// the first sample of the block becomes audible. Negative = frozen
    /// anchor (no extrapolation allowed).
    latency_ns: AtomicI64,
}

impl Default for PlayheadAnchor {
    fn default() -> Self {
        Self::new()
    }
}

impl PlayheadAnchor {
    /// Anchor with the process epoch pinned. Pinning here (not lazily on
    /// first use) guarantees every later `Instant` captured by callers is
    /// ≥ the epoch, so `instant_ns` never collapses an anchor time to 0.
    #[must_use]
    pub fn new() -> Self {
        EPOCH.get_or_init(Instant::now);
        Self {
            seq: AtomicU32::new(0),
            first_frame: AtomicU64::new(0),
            at_ns: AtomicU64::new(0),
            latency_ns: AtomicI64::new(LATENCY_INVALID),
        }
    }

    /// Publish a fresh anchor from the audio callback.
    ///
    /// * `first_frame` — frame position of the first sample popped in this
    ///   callback (what [`audible_position_seconds`] extrapolates from);
    /// * `at` — `Instant::now()` captured in the callback (the UI clock);
    /// * `latency` — measured callback→audible delay. Host-reported when
    ///   the backend provides `playback − callback` (ALSA delay, WASAPI
    ///   padding, CoreAudio), else the caller's measured fallback.
    ///
    /// # Real-time safety
    /// No allocation, no lock, no syscall beyond `Instant::now()` (vDSO
    /// on Linux, QPC on Windows, mach_absolute_time on macOS).
    pub fn update(&self, first_frame: u64, at: Instant, latency: Duration) {
        let s = self.seq.load(Ordering::Relaxed);
        // Begin write: seq odd. Release so the payload stores cannot be
        // reordered before it.
        self.seq.store(s.wrapping_add(1), Ordering::Release);
        self.first_frame.store(first_frame, Ordering::Release);
        self.at_ns.store(instant_ns(at), Ordering::Release);
        let lat = i64::try_from(latency.as_nanos()).unwrap_or(i64::MAX);
        self.latency_ns.store(lat, Ordering::Release);
        // End write: seq even again (s + 2). Single-writer protocol.
        self.seq.store(s.wrapping_add(2), Ordering::Release);
    }

    /// Invalidate the anchor: freeze at `frame` and refuse to extrapolate.
    /// Called by the feeder on play/stop/seek generation bumps so the
    /// playhead cannot run ahead of audio that has not been handed yet.
    pub fn reset_frozen(&self, frame: u64) {
        let s = self.seq.load(Ordering::Relaxed);
        self.seq.store(s.wrapping_add(1), Ordering::Release);
        self.first_frame.store(frame, Ordering::Release);
        self.at_ns.store(now_ns(), Ordering::Release);
        self.latency_ns.store(LATENCY_INVALID, Ordering::Release);
        self.seq.store(s.wrapping_add(2), Ordering::Release);
    }

    /// Consistent read of the anchor payload (bounded seqlock retries;
    /// `None` when the writer keeps winning — callers fall back to the
    /// handed cursor, the honest upper bound of the audible position).
    #[must_use]
    fn read(&self) -> Option<(u64, u64, i64)> {
        for attempt in 0..32 {
            let s1 = self.seq.load(Ordering::Acquire);
            if s1 & 1 == 1 {
                // Writer mid-update: spin briefly, then yield the core so
                // a UI-priority reader cannot burn quota against an
                // RT-priority writer.
                if attempt % 8 == 7 {
                    std::thread::yield_now();
                } else {
                    std::hint::spin_loop();
                }
                continue;
            }
            let frame = self.first_frame.load(Ordering::Acquire);
            let at = self.at_ns.load(Ordering::Acquire);
            let lat = self.latency_ns.load(Ordering::Acquire);
            if s1 == self.seq.load(Ordering::Acquire) {
                return Some((frame, at, lat));
            }
        }
        None
    }

    /// Last anchored frame position (the frozen/resume point).
    #[must_use]
    pub fn frozen_frame(&self) -> u64 {
        self.read().map_or(0, |(frame, _, _)| frame)
    }

    /// Whether the anchor currently allows extrapolation (a live audio
    /// callback has published since the last reset).
    #[must_use]
    pub fn is_extrapolatable(&self) -> bool {
        self.read()
            .is_some_and(|(_, _, lat)| lat != LATENCY_INVALID)
    }
}

/// Compute the audible playhead position in seconds.
///
/// Shared implementation of both players' `position_seconds`: anchor-based
/// extrapolation when an audio callback has published, clamped to the
/// handed cursor and the track length, falling back to the handed cursor
/// itself when nothing audible can be derived yet.
///
/// * `anchor` — the shared seqlock anchor;
/// * `rate` — sample rate of the *playback stream* (the adapted/device
///   rate, not the file rate — all counters are in stream frames);
/// * `channels` — stream channel count (`base`/`consumed` are interleaved
///   samples, the anchor is in frames);
/// * `base`/`consumed` — the lock-free handed-cursor counters (samples):
///   position of the first sample pushed after the last generation bump,
///   and samples handed to the device since;
/// * `length_seconds` — track duration (clamps the top);
/// * `playing` — when false the playhead freezes at the last anchor
///   (the resume point) instead of extrapolating;
/// * `now` — the caller's clock reading (UI thread).
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn audible_position_seconds(
    anchor: &PlayheadAnchor,
    rate: f64,
    channels: u64,
    base: u64,
    consumed: u64,
    length_seconds: f64,
    playing: bool,
    now: Instant,
) -> f64 {
    let rate = if rate > 0.0 { rate } else { 1.0 };
    let channels = channels.max(1);
    let handed_s = (base.saturating_add(consumed) / channels) as f64 / rate;
    // Only NaN / negative lengths are invalid; `INFINITY` legitimately
    // means "unbounded" (clamp no-op).
    let length_seconds = if length_seconds.is_nan() || length_seconds < 0.0 {
        handed_s
    } else {
        length_seconds
    };

    let (frame, at, lat) = match anchor.read() {
        Some(v) => v,
        None => return handed_s.clamp(0.0, length_seconds),
    };

    if !playing || lat == LATENCY_INVALID {
        // Frozen: the position audio will resume from (or parked at after
        // stop — the feeder resets the anchor to the new base, so a stop
        // freezes at 0 and a seek freezes at the target).
        return (frame as f64 / rate).clamp(0.0, length_seconds);
    }

    let now_n = instant_ns(now);
    let elapsed_s = now_n.saturating_sub(at) as f64 / 1e9;
    let latency_s = lat as f64 / 1e9;
    // aud(T) = first_frame + (T − callback − latency) · rate. Between the
    // callback and `latency` elapsing this correctly shows the tail of the
    // previous block (a position slightly *before* `first_frame`).
    let audible_s = (frame as f64 + (elapsed_s - latency_s) * rate) / rate;
    // Never lead the handed cursor (underruns freeze instead of running
    // ahead of silence) and never leave the track. All bounds in seconds.
    audible_s.clamp(0.0, handed_s).clamp(0.0, length_seconds)
}

/// Resolve the device latency from a cpal output timestamp pair.
///
/// Returns the host-reported `playback − callback` duration when the
/// backend predicts a playback instant later than the callback instant
/// (ALSA `snd_pcm_delay`, WASAPI padding, CoreAudio), else `None` — the
/// caller then substitutes its own measured fallback.
#[must_use]
pub fn host_output_latency(ts: &cpal::OutputStreamTimestamp) -> Option<Duration> {
    ts.playback
        .checked_duration_since(ts.callback)
        .filter(|d| !d.is_zero())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anchor_with(frame: u64, latency_ms: f64) -> (PlayheadAnchor, Instant) {
        let a = PlayheadAnchor::new();
        let at = Instant::now();
        a.update(frame, at, Duration::from_secs_f64(latency_ms / 1000.0));
        (a, at)
    }

    #[test]
    fn extrapolation_matches_device_time() {
        // Anchor: frame 48_000 @ 48 kHz becomes audible 21 ms after the
        // callback. Half a second later the audible position must be
        // 1.0 s + (0.5 − 0.021) s. The handed cursor must be consistent
        // with the scenario (1.5 s handed by later callbacks).
        let (a, at) = anchor_with(48_000, 21.0);
        let later = at + Duration::from_millis(500);
        let pos = audible_position_seconds(&a, 48_000.0, 1, 0, 72_000, f64::INFINITY, true, later);
        let expected = 1.0 + (0.5 - 0.021);
        assert!(
            (pos - expected).abs() < 0.002,
            "pos {pos} vs expected {expected}"
        );
    }

    #[test]
    fn latency_pulls_playhead_behind_handed_cursor() {
        // Anchor at frame 0 with 40 ms device latency: right at the
        // callback the audible position is −40 ms → clamped to 0 (the
        // previous block tail), i.e. strictly behind the handed cursor.
        let (a, at) = anchor_with(0, 40.0);
        let pos = audible_position_seconds(&a, 48_000.0, 1, 0, 4_800, f64::INFINITY, true, at);
        assert!(pos <= 0.001 + f64::EPSILON, "pos {pos} must not lead audio");
    }

    #[test]
    fn playhead_never_leads_handed_cursor() {
        let (a, at) = anchor_with(48_000, 5.0);
        // Handed cursor frozen at 1.0 s (no further callbacks): 10 s of
        // wall time must not push the playhead past 1.0 s.
        let later = at + Duration::from_secs(10);
        let pos = audible_position_seconds(&a, 48_000.0, 1, 0, 48_000, f64::INFINITY, true, later);
        assert!(
            (pos - 1.0).abs() < 1e-9,
            "pos {pos} must clamp to the handed cursor 1.0"
        );
    }

    #[test]
    fn frozen_when_not_playing() {
        let (a, at) = anchor_with(24_000, 10.0);
        let later = at + Duration::from_secs(5);
        let pos = audible_position_seconds(&a, 48_000.0, 1, 0, 0, f64::INFINITY, false, later);
        let expected = 0.5;
        assert!(
            (pos - expected).abs() < 1e-9,
            "paused playhead must freeze at the anchor frame ({pos} vs {expected})"
        );
    }

    #[test]
    fn reset_frozen_stops_extrapolation() {
        let (a, _at) = anchor_with(48_000, 10.0);
        assert!(a.is_extrapolatable());
        // Seek to 2.0 s: anchor frozen at the new base.
        a.reset_frozen(96_000);
        assert!(!a.is_extrapolatable());
        assert_eq!(a.frozen_frame(), 96_000);
        // Frozen anchor → the playhead parks at the seek target (2.0 s)
        // until the next real callback publishes a live anchor, even
        // though the caller claims "playing" and time advances.
        let pos = audible_position_seconds(
            &a,
            48_000.0,
            1,
            96_000,
            0,
            f64::INFINITY,
            true,
            Instant::now(),
        );
        assert!(
            (pos - 2.0).abs() < 1e-9,
            "pos {pos} must freeze at the seek target"
        );
    }

    #[test]
    fn clamps_to_track_length() {
        // Handed 4 s (consistent with the 100 s of extrapolated time),
        // track 3.5 s → the length clamp wins.
        let (a, at) = anchor_with(0, 1.0);
        let later = at + Duration::from_secs(100);
        let pos = audible_position_seconds(&a, 48_000.0, 1, 0, 192_000, 3.5, true, later);
        assert!((pos - 3.5).abs() < 1e-9, "pos {pos} must clamp to length");
    }

    #[test]
    fn channels_divide_samples_into_frames() {
        // Stereo: base 96_000 samples + 4_800 consumed = 50_400 frames.
        let a = PlayheadAnchor::new();
        a.reset_frozen(50_400);
        let pos = audible_position_seconds(
            &a,
            48_000.0,
            2,
            96_000,
            4_800,
            f64::INFINITY,
            false,
            Instant::now(),
        );
        assert!((pos - 1.05).abs() < 1e-9, "pos {pos}");
    }

    /// Hammer the writer from one thread while the reader checks that it
    /// never observes a torn triple. The writer runs at ~10 kHz — ten
    /// times a real device callback rate — so contention is stressed
    /// without degenerating into a seqlock starvation loop no real audio
    /// thread would ever produce.
    #[test]
    fn seqlock_reader_never_torn_under_write_pressure() {
        let a = std::sync::Arc::new(PlayheadAnchor::new());
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let writer = {
            let a = std::sync::Arc::clone(&a);
            let stop = std::sync::Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut frame = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    frame += 48;
                    a.update(frame, Instant::now(), Duration::from_millis(10));
                    std::thread::sleep(Duration::from_micros(100));
                }
            })
        };
        let mut last_pos = 0.0f64;
        for _ in 0..20_000 {
            // Handed cursor far ahead (consistent with a real stream mid
            // playback) so the extrapolation path — not the clamp — is
            // what the reader exercises.
            let pos = audible_position_seconds(
                &a,
                48_000.0,
                1,
                0,
                4_800_000,
                f64::INFINITY,
                true,
                Instant::now(),
            );
            // Torn reads would surface as absurd jumps; anchor frames only
            // ever grow by 48 here, so allow generous real-time skew but
            // catch negative-frames garbage.
            assert!(pos >= last_pos - 1.0, "torn read: {last_pos} → {pos}");
            last_pos = pos;
        }
        stop.store(true, Ordering::Relaxed);
        writer.join().unwrap();
    }

    #[test]
    fn host_output_latency_extracts_positive_delta() {
        let cb = cpal::StreamInstant::from_millis(100);
        let pb = cpal::StreamInstant::from_millis(121);
        let ts = cpal::OutputStreamTimestamp {
            callback: cb,
            playback: pb,
        };
        assert_eq!(host_output_latency(&ts), Some(Duration::from_millis(21)));
        // Equal instants (no host prediction) → None.
        let ts2 = cpal::OutputStreamTimestamp {
            callback: cb,
            playback: cb,
        };
        assert_eq!(host_output_latency(&ts2), None);
        // Playback earlier than callback (nonsensical host) → None.
        let ts3 = cpal::OutputStreamTimestamp {
            callback: pb,
            playback: cb,
        };
        assert_eq!(host_output_latency(&ts3), None);
    }
}
