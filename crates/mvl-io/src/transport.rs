//! Transport state machine — play / pause / stop / seek.
//!
//! Pure logic, no threads and no audio hardware: the player feeder thread
//! drives it, and the whole behavior is unit-testable headless.

use std::time::Duration;

/// Playback state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TransportState {
    /// Not playing; position at start; ring drained.
    #[default]
    Stopped,
    /// Playing; position advances with [`Transport::advance`].
    Playing,
    /// Frozen mid-track; resume continues from the same position.
    Paused,
}

/// Source-length playhead bookkeeping with idempotent transport controls.
///
/// All position values are in *frames* (one frame = one sample per
/// channel). The owning player layer converts to/from seconds.
#[derive(Debug, Clone)]
pub struct Transport {
    state: TransportState,
    position: u64,
    length: u64,
}

impl Transport {
    /// New transport for a track of `length` frames.
    ///
    /// # Panics
    /// Never; a zero-length track simply refuses to play.
    #[must_use]
    pub fn new(length: u64) -> Self {
        Self {
            state: TransportState::Stopped,
            position: 0,
            length,
        }
    }

    /// Current state.
    #[must_use]
    pub fn state(&self) -> TransportState {
        self.state
    }

    /// True while playing.
    #[must_use]
    pub fn is_playing(&self) -> bool {
        self.state == TransportState::Playing
    }

    /// Playhead position in frames.
    #[must_use]
    pub fn position(&self) -> u64 {
        self.position
    }

    /// Track length in frames.
    #[must_use]
    pub fn length(&self) -> u64 {
        self.length
    }

    /// Start (or resume) playback. Idempotent: playing while playing is a
    /// no-op. Starting a finished track restarts from zero.
    pub fn play(&mut self) {
        if self.length == 0 {
            return;
        }
        match self.state {
            TransportState::Stopped => {
                self.position = 0;
                self.state = TransportState::Playing;
            }
            TransportState::Paused => self.state = TransportState::Playing,
            TransportState::Playing => {}
        }
    }

    /// Pause playback. Idempotent.
    pub fn pause(&mut self) {
        if self.state == TransportState::Playing {
            self.state = TransportState::Paused;
        }
    }

    /// Stop playback and rewind to zero. Idempotent.
    pub fn stop(&mut self) {
        self.state = TransportState::Stopped;
        self.position = 0;
    }

    /// Jump to `seconds` (clamped to the track). Works in any state.
    pub fn seek_seconds(&mut self, seconds: f64, sample_rate: u32) {
        let frames = (seconds.max(0.0) * f64::from(sample_rate)).round() as u64;
        self.seek_frames(frames);
    }

    /// Jump to `frames` (clamped to the track). Works in any state.
    pub fn seek_frames(&mut self, frames: u64) {
        self.position = frames.min(self.length);
    }

    /// Advance the playhead by `n` frames while playing. Reaching the end
    /// transitions to [`TransportState::Stopped`] (position stays at
    /// length so callers can show "finished").
    pub fn advance(&mut self, n: u64) {
        if self.state != TransportState::Playing {
            return;
        }
        self.position = self.position.saturating_add(n);
        if self.position >= self.length {
            self.position = self.length;
            self.state = TransportState::Stopped;
        }
    }

    /// Position as `Duration` at `sample_rate`.
    #[must_use]
    pub fn position_duration(&self, sample_rate: u32) -> Duration {
        Duration::from_secs_f64(self.position as f64 / f64::from(sample_rate))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn play_pause_resume_cycle() {
        let mut t = Transport::new(1_000);
        assert_eq!(t.state(), TransportState::Stopped);
        t.play();
        assert_eq!(t.state(), TransportState::Playing);
        t.advance(300);
        assert_eq!(t.position(), 300);
        t.pause();
        assert_eq!(t.state(), TransportState::Paused);
        // Paused: advance is a no-op.
        t.advance(500);
        assert_eq!(t.position(), 300);
        t.play();
        t.advance(100);
        assert_eq!(t.position(), 400);
    }

    #[test]
    fn stop_rewinds() {
        let mut t = Transport::new(1_000);
        t.play();
        t.advance(700);
        t.stop();
        assert_eq!(t.state(), TransportState::Stopped);
        assert_eq!(t.position(), 0);
        t.play();
        assert_eq!(t.position(), 0);
    }

    #[test]
    fn reaching_end_stops() {
        let mut t = Transport::new(100);
        t.play();
        t.advance(60);
        t.advance(60);
        assert_eq!(t.state(), TransportState::Stopped);
        assert_eq!(t.position(), 100, "position parks at length when finished");
        // Restarting after natural end begins from zero.
        t.play();
        assert_eq!(t.position(), 0);
    }

    #[test]
    fn seek_clamps_and_works_when_stopped() {
        let mut t = Transport::new(10_000);
        t.seek_frames(5_000);
        assert_eq!(t.position(), 5_000);
        t.seek_frames(999_999);
        assert_eq!(t.position(), 10_000, "seek clamps to length");
        t.seek_seconds(-5.0, 48_000);
        assert_eq!(t.position(), 0, "negative seek clamps to zero");
        t.seek_seconds(0.5, 48_000);
        // 24_000 frames would be past the 10_000-frame track: clamps.
        assert_eq!(t.position(), 10_000);
    }

    #[test]
    fn seek_while_paused_resumes_there() {
        let mut t = Transport::new(48_000);
        t.play();
        t.advance(1_000);
        t.pause();
        t.seek_seconds(0.25, 48_000);
        assert_eq!(t.position(), 12_000);
        t.play();
        t.advance(1);
        assert_eq!(t.position(), 12_001);
    }

    #[test]
    fn zero_length_track_never_plays() {
        let mut t = Transport::new(0);
        t.play();
        assert_eq!(t.state(), TransportState::Stopped);
        t.advance(100);
        assert_eq!(t.position(), 0);
    }

    #[test]
    fn position_duration_conversion() {
        let mut t = Transport::new(96_000);
        t.play();
        t.advance(48_000);
        assert_eq!(t.position_duration(48_000), Duration::from_secs(1));
    }
}
