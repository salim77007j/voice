//! Waveform rendering (plan §8.4): peak mipmap pyramid, offscreen canvas
//! rendering into a `SharedPixelBuffer`, adaptive ruler ticks and zoom math.
//!
//! The mipmap is computed once per file; zooming selects the level whose
//! block density is ≥ 1 block per pixel so rendering is O(pixels) at every
//! zoom level. At maximum zoom (sub-millisecond spans) individual samples
//! are drawn as a stem plot (plan requirement R8).

use slint::{Image, Rgba8Pixel, SharedPixelBuffer};

/// Colour constants kept in lockstep with `ui/theme.slint`.
const PEAK: [u8; 4] = [74, 144, 226, 255]; // #4A90E2 (audio-data blue)
const RMS: [u8; 4] = [74, 144, 226, 140]; // #4A90E2 @55 %
const CENTER: [u8; 4] = [255, 255, 255, 10]; // subtle center guide
/// All-zero RGBA (used by tests to assert untouched pixels).
#[cfg_attr(not(test), allow(dead_code))]
const TRANSPARENT: [u8; 4] = [0, 0, 0, 0];

/// Downsampling factor between mipmap levels.
const LEVEL_STRIDE: usize = 4;

/// One mipmap level: min / max / RMS per block of samples (mono-mixed).
struct MipLevel {
    block: usize,
    min: Vec<f32>,
    max: Vec<f32>,
    rms: Vec<f32>,
}

impl MipLevel {
    fn len(&self) -> usize {
        self.min.len()
    }
}

/// Peak pyramid over a mono-mixed signal.
pub struct PeakMipmap {
    levels: Vec<MipLevel>,
    frames: usize,
}

impl PeakMipmap {
    /// Build the pyramid: level 0 has per-sample values, each following
    /// level aggregates `LEVEL_STRIDE` blocks.
    #[must_use]
    pub fn build(samples: &[f32], channels: usize) -> Self {
        debug_assert!(channels >= 1);
        // mono mix first (the waveform display is a single trace)
        let mono: Vec<f32> = if channels == 1 {
            samples.to_vec()
        } else {
            samples
                .chunks(channels)
                .map(|frame| frame.iter().sum::<f32>() / channels as f32)
                .collect()
        };

        let mut levels = Vec::new();
        // level 0: one block per sample
        let min: Vec<f32> = mono.clone();
        let max = min.clone();
        let rms = min.clone();
        let frames = mono.len();
        levels.push(MipLevel {
            block: 1,
            min,
            max,
            rms,
        });

        while levels.last().is_some_and(|l| l.len() > 1) {
            let prev = levels.last().expect("checked");
            let block = prev.block * LEVEL_STRIDE;
            let n = prev.len().div_ceil(LEVEL_STRIDE);
            let mut min = Vec::with_capacity(n);
            let mut max = Vec::with_capacity(n);
            let mut rms = Vec::with_capacity(n);
            for i in 0..n {
                let s = i * LEVEL_STRIDE;
                let e = (s + LEVEL_STRIDE).min(prev.len());
                // aggregate each statistic from its own column: peaks
                // must survive every level (min-of-min, max-of-max); RMS
                // combines as the root of the mean of squared block RMS.
                let mut lo = f32::INFINITY;
                let mut hi = f32::NEG_INFINITY;
                let mut sum_sq = 0.0f64;
                for b in s..e {
                    lo = lo.min(prev.min[b]);
                    hi = hi.max(prev.max[b]);
                    sum_sq += f64::from(prev.rms[b]) * f64::from(prev.rms[b]);
                }
                min.push(lo);
                max.push(hi);
                rms.push((sum_sq / (e - s) as f64).sqrt() as f32);
            }
            levels.push(MipLevel {
                block,
                min,
                max,
                rms,
            });
        }

        Self { levels, frames }
    }

    /// Number of source frames covered.
    #[must_use]
    pub fn frames(&self) -> usize {
        self.frames
    }

    /// Deepest level whose block density covers at least one block per
    /// output column — the level that makes rendering O(pixels).
    fn level_for(&self, view_frames: f64, width_px: f64) -> &MipLevel {
        if width_px < 1.0 {
            return self.levels.first().expect("always level 0");
        }
        let frames_per_px = view_frames / width_px;
        // choose the coarsest level that still has ≤ `frames_per_px`
        // frames per block (finer than needed wastes work; coarser loses
        // peaks)
        let mut best = self.levels.first().expect("always level 0");
        for level in &self.levels {
            if level.block as f64 <= frames_per_px {
                best = level;
            } else {
                break;
            }
        }
        best
    }
}

/// Render the waveform for a frame range into a new pixel buffer and
/// wrap it as a UI image.
///
/// `view_start`/`view_end` are frame indices into the source; the buffer
/// is `width`×`height` with a transparent background (the panel colour
/// shows through in the UI).
#[must_use]
pub fn render_waveform(
    mipmap: &PeakMipmap,
    channels: usize,
    view_start: usize,
    view_end: usize,
    width: usize,
    height: usize,
    sample_rate: u32,
) -> Image {
    Image::from_rgba8(render_waveform_buffer(
        mipmap,
        channels,
        view_start,
        view_end,
        width,
        height,
        sample_rate,
    ))
}

/// [`render_waveform`] without the `Image` wrapper — returns the raw
/// buffer so tests can assert on exact pixels.
#[must_use]
pub fn render_waveform_buffer(
    mipmap: &PeakMipmap,
    channels: usize,
    view_start: usize,
    view_end: usize,
    width: usize,
    height: usize,
    sample_rate: u32,
) -> SharedPixelBuffer<Rgba8Pixel> {
    let _ = channels; // the mipmap is already mono-mixed
    let width = width.max(1);
    let height = height.max(1);
    let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(width as u32, height as u32);

    // BUG 2 fix (crash on import): an empty recording/import produced a
    // zero-frame mipmap, and the old `view_end.clamp(view_start + 1,
    // mipmap.frames())` degenerated to `clamp(1, 0)` — a guaranteed
    // `min > max` panic that killed the app the moment such a file was
    // loaded. Empty audio now renders an empty (transparent) canvas
    // instead of panicking.
    if mipmap.frames() == 0 {
        return buffer;
    }

    let view_start = view_start.min(mipmap.frames());
    let view_end = if mipmap.frames() > view_start + 1 {
        view_end.clamp(view_start + 1, mipmap.frames())
    } else {
        mipmap.frames()
    };
    let view_frames = (view_end - view_start) as f64;

    // sample-level stem plot when fewer samples than ~2 columns
    let sample_level = view_frames / width as f64 <= 2.0;

    let cy = height as f64 / 2.0;
    let half = height as f64 / 2.0 - 2.0;

    let px = buffer.make_mut_bytes();
    let stride = width * 4;

    // center guide
    if height > 4 {
        let y = height / 2;
        for x in 0..width {
            put(px, stride, x, y, CENTER);
        }
    }

    if sample_level {
        // stem plot: one stem per column from the exact samples
        for x in 0..width {
            let f0 = view_start as f64 + x as f64 * view_frames / width as f64;
            let f1 = f0 + view_frames / width as f64;
            let s = f0.round() as usize;
            let e = (f1.round() as usize).max(s + 1).min(view_end);
            let mut lo = 0.0f32;
            let mut hi = 0.0f32;
            for f in s..e {
                let v = sample_or_zero(mipmap, f);
                lo = lo.min(v);
                hi = hi.max(v);
            }
            draw_column(px, stride, x, height, cy, half, lo, hi, (lo + hi) * 0.5);
        }
    } else {
        let level = mipmap.level_for(view_frames, width as f64);
        let blocks_per_px = level.len() as f64 / width as f64;
        for x in 0..width {
            let b0 = (x as f64 * blocks_per_px).floor() as usize;
            let b1 = ((x + 1) as f64 * blocks_per_px).ceil() as usize;
            let b1 = b1.max(b0 + 1).min(level.len());
            let mut lo = f32::INFINITY;
            let mut hi = f32::NEG_INFINITY;
            let mut rms = 0.0f32;
            for b in b0..b1 {
                lo = lo.min(level.min[b]);
                hi = hi.max(level.max[b]);
                rms = rms.max(level.rms[b]);
            }
            if lo > hi {
                lo = 0.0;
                hi = 0.0;
            }
            draw_column(px, stride, x, height, cy, half, lo, hi, rms);
        }
    }

    let _ = sample_rate; // reserved for future per-sample time labels
    buffer
}

fn sample_or_zero(mipmap: &PeakMipmap, frame: usize) -> f32 {
    let l0 = &mipmap.levels[0];
    if frame < l0.len() {
        l0.min[frame]
    } else {
        0.0
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_column(
    px: &mut [u8],
    stride: usize,
    x: usize,
    height: usize,
    cy: f64,
    half: f64,
    lo: f32,
    hi: f32,
    rms: f32,
) {
    if height < 2 {
        return; // nothing sensible to draw at this size
    }
    let max_y = height as i64 - 1;
    let half = half.max(0.0);

    // RMS body from −rms..+rms: alpha fades from 190 at the center line
    // to 60 at the body edge — the analog "weight near the baseline"
    // look (Phase 7.2) instead of a flat solid fill.
    let r = (f64::from(rms.clamp(-1.0, 1.0).abs()) * half).round() as i64;
    let body_top = (cy as i64 - r).clamp(0, max_y) as usize;
    let body_bot = (cy as i64 + r).clamp(0, max_y) as usize;
    for y in body_top..=body_bot {
        // 0 at the center, 1 at the body edge
        let t = (y as f64 - cy).abs() / r.max(1) as f64;
        let alpha = 190.0 - 130.0 * t.clamp(0.0, 1.0);
        let mut px_rgba = RMS;
        px_rgba[3] = alpha.round() as u8;
        put(px, stride, x, y, px_rgba);
    }

    // peak outline (full blue): lo/hi map to rows, clamped to the canvas
    let top =
        ((cy - f64::from(hi.clamp(-1.0, 1.0)) * half).round() as i64).clamp(0, max_y) as usize;
    let bot =
        ((cy - f64::from(lo.clamp(-1.0, 1.0)) * half).round() as i64).clamp(0, max_y) as usize;

    // connect the outline to the body so thin peaks stay visible
    for y in (top as i64)..(body_top as i64) {
        if (0..=max_y).contains(&y) {
            put(px, stride, x, y as usize, PEAK);
        }
    }
    for y in (body_bot as i64 + 1)..=(bot as i64) {
        if (0..=max_y).contains(&y) {
            put(px, stride, x, y as usize, PEAK);
        }
    }
    put(px, stride, x, top, PEAK);
    put(px, stride, x, bot, PEAK);
}

fn put(px: &mut [u8], stride: usize, x: usize, y: usize, rgba: [u8; 4]) {
    let o = y * stride + x * 4;
    if o + 3 < px.len() {
        px[o..o + 4].copy_from_slice(&rgba);
    }
}

// ---------------------------------------------------------------------------
// Ruler ticks
// ---------------------------------------------------------------------------

/// A ruler tick in view-relative coordinates.
#[derive(Debug, Clone, PartialEq)]
pub struct Tick {
    /// 0..1 across the canvas width.
    pub fraction: f32,
    /// Locale-neutral label (mono font, Western digits per plan §8.6).
    pub label: String,
    /// Major ticks span the ruler height.
    pub major: bool,
}

/// Nice intervals (1-2-5 ladder) in seconds.
const NICE: [f64; 15] = [
    0.000_01, 0.000_02, 0.000_05, 0.000_1, 0.000_2, 0.000_5, 0.001, 0.002, 0.005, 0.01, 0.05, 0.1,
    0.5, 1.0, 5.0,
];

/// Adaptive ruler ticks for a view span: choose the smallest nice interval
/// whose pixel distance is at least `min_px` (readable labels), mark every
/// 5th tick major.
#[must_use]
pub fn ruler_ticks(view_start: f64, view_end: f64, width_px: f64, min_px: f64) -> Vec<Tick> {
    let span = (view_end - view_start).max(f64::EPSILON);
    let width = width_px.max(1.0);
    let interval = *NICE
        .iter()
        .find(|&&i| i / span * width >= min_px)
        .unwrap_or(&NICE[NICE.len() - 1]);

    let first = (view_start / interval).ceil() as i64;
    let last = (view_end / interval).floor() as i64;
    let mut ticks = Vec::new();
    // every 5th tick is major — anchored so the origin is major
    for k in first..=last {
        let t = k as f64 * interval;
        let fraction = ((t - view_start) / span) as f32;
        if !(0.0..=1.0).contains(&fraction) {
            continue;
        }
        ticks.push(Tick {
            fraction,
            label: tick_label(t),
            major: k.rem_euclid(5) == 0,
        });
    }
    ticks
}

fn tick_label(t: f64) -> String {
    if t < 0.001 {
        format!("{:.2} ms", t * 1000.0)
    } else if t < 1.0 {
        format!("{:.0} ms", t * 1000.0)
    } else if t < 60.0 {
        format!("{t:.2} s")
    } else {
        let m = t.trunc() as u64 / 60;
        let s = t.trunc() as u64 % 60;
        format!("{m:02}:{s:02}")
    }
}

// ---------------------------------------------------------------------------
// Zoom math
// ---------------------------------------------------------------------------

/// A view span in seconds, clamped to the track bounds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ViewSpan {
    pub start: f64,
    pub end: f64,
}

impl ViewSpan {
    /// Full-file span.
    #[must_use]
    pub fn full(duration: f64) -> Self {
        Self {
            start: 0.0,
            end: duration.max(1e-6),
        }
    }

    /// Clamp so that `0 ≤ start < end ≤ duration` and the span respects
    /// the minimum width (8 samples).
    #[must_use]
    pub fn clamped(self, duration: f64, sample_rate: u32) -> Self {
        let duration = duration.max(1e-6);
        let min_span = (8.0 / sample_rate.max(1) as f64).max(1e-6);
        let mut end = self.end.min(duration);
        let mut start = self.start.clamp(0.0, end);
        let span = (end - start).max(min_span);
        if start + span > duration {
            start = (duration - span).max(0.0);
        }
        end = (start + span).min(duration);
        if end - start < min_span {
            end = (start + min_span).min(duration);
        }
        Self { start, end }
    }

    /// Zoom around an anchor fraction (0..1 of the current view) by a
    /// factor (e.g. 0.6 in, 1.6 out).
    #[must_use]
    pub fn zoomed(self, anchor: f64, factor: f64, duration: f64, sample_rate: u32) -> Self {
        let anchor = anchor.clamp(0.0, 1.0);
        let span = self.end - self.start;
        let at = self.start + span * anchor;
        let new_span = (span * factor).max(1e-9);
        let start = at - (at - self.start) * new_span / span.max(1e-9);
        Self {
            start,
            end: start + new_span,
        }
        .clamped(duration, sample_rate)
    }

    /// True when the span shows individual samples (sub-millisecond /
    /// sample-level indicator, plan §8.2).
    #[must_use]
    pub fn sample_level(self, sample_rate: u32) -> bool {
        (self.end - self.start) <= 8.0 / sample_rate.max(1) as f64 * 8.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(frames: usize, freq: f64, rate: f64) -> Vec<f32> {
        (0..frames)
            .map(|i| ((2.0 * std::f64::consts::PI * freq * i as f64 / rate).sin() * 0.8) as f32)
            .collect()
    }

    #[test]
    fn mipmap_levels_shrink() {
        let m = PeakMipmap::build(&sine(1000, 440.0, 48_000.0), 1);
        assert_eq!(m.frames(), 1000);
        assert_eq!(m.levels[0].len(), 1000);
        assert_eq!(m.levels[1].len(), 250);
        assert_eq!(m.levels[2].len(), 63);
        // last level reaches a single block
        let last = m.levels.last().unwrap();
        assert_eq!(last.len(), 1);
    }

    #[test]
    fn mipmap_preserves_extremes() {
        // a single loud sample in silence must be visible at every level
        let mut samples = vec![0.0f32; 4096];
        samples[1234] = 0.9;
        let m = PeakMipmap::build(&samples, 1);
        for level in &m.levels {
            let hi = level.max.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
            assert!(
                (hi - 0.9).abs() < 1e-6,
                "peak lost at block {}",
                level.block
            );
        }
    }

    #[test]
    fn mipmap_stereo_mixes_channels() {
        // L = +0.5 constant, R = −0.5 constant → mono ≈ 0
        let data: Vec<f32> = (0..64).flat_map(|_| [0.5, -0.5]).collect();
        let m = PeakMipmap::build(&data, 2);
        assert_eq!(m.frames(), 64);
        assert!(m.levels[0].max.iter().all(|&v| v.abs() < 1e-6));
    }

    #[test]
    fn render_is_deterministic_and_blue() {
        let m = PeakMipmap::build(&sine(48_000, 440.0, 48_000.0), 1);
        let a = render_waveform_buffer(&m, 1, 0, 48_000, 400, 200, 48_000);
        let b = render_waveform_buffer(&m, 1, 0, 48_000, 400, 200, 48_000);
        assert_eq!((a.width(), a.height()), (400, 200));
        // deterministic: same input → same buffer
        let ba = a.as_bytes().to_vec();
        let bb = b.as_bytes().to_vec();
        assert_eq!(ba, bb);

        // background transparent at the very top, wave pixels blue-ish in
        // the middle band: sample a column near the loud centre
        let stride = 400 * 4;
        let top = &ba[0..4];
        assert_eq!(top, TRANSPARENT, "top corner must be transparent");
        let mid = &ba[100 * stride + 200 * 4..100 * stride + 200 * 4 + 4];
        // at the centre line we either drew the guide, the body (gradient
        // alpha 190 at the centre) or the peak outline
        let rms_center = [RMS[0], RMS[1], RMS[2], 190];
        assert!(
            mid == CENTER || mid == rms_center || mid == PEAK,
            "centre pixel should be part of the trace, got {mid:?}"
        );
    }

    #[test]
    fn render_sample_level_stem() {
        // 16 samples over 100 columns → stem plot path
        let m = PeakMipmap::build(&sine(16, 440.0, 48_000.0), 1);
        let img = render_waveform_buffer(&m, 1, 0, 16, 100, 100, 48_000);
        assert_eq!((img.width(), img.height()), (100, 100));
        let bytes = img.as_bytes();
        // the drawn stems must contain blue pixels
        assert!(
            bytes
                .chunks_exact(4)
                .any(|p| p[0] == PEAK[0] && p[1] == PEAK[1]),
            "stem plot must draw blue pixels"
        );
    }

    #[test]
    fn render_empty_mipmap_does_not_panic() {
        // BUG 2 regression: a zero-frame recording/import used to hit
        // `clamp(1, 0)` and kill the app. The empty mipmap must render an
        // empty canvas instead.
        let m = PeakMipmap::build(&[], 1);
        assert_eq!(m.frames(), 0);
        let img = render_waveform_buffer(&m, 1, 0, 1, 400, 200, 48_000);
        assert_eq!((img.width(), img.height()), (400, 200));
        assert!(
            img.as_bytes().iter().all(|&b| b == 0),
            "empty audio renders a fully transparent canvas"
        );
    }

    #[test]
    fn render_single_frame_mipmap_does_not_panic() {
        // 1-frame mipmap: view_start.min(1) == view_end == frames() — the
        // other degenerate clamp boundary.
        let m = PeakMipmap::build(&[0.5f32], 1);
        assert_eq!(m.frames(), 1);
        let img = render_waveform_buffer(&m, 1, 0, 1, 400, 200, 48_000);
        assert_eq!((img.width(), img.height()), (400, 200));
    }

    #[test]
    fn ticks_respect_min_spacing() {
        // 10 s over 1000 px → interval 1 s (100 px) … check labels
        let ticks = ruler_ticks(0.0, 10.0, 1000.0, 70.0);
        assert!(!ticks.is_empty());
        // every tick at least 60 px apart
        for w in ticks.windows(2) {
            let d = (w[1].fraction - w[0].fraction).abs() * 1000.0;
            assert!(d >= 69.0, "ticks too close: {d:.1} px");
        }
        assert!(ticks.iter().any(|t| t.label == "1.00 s"));
        assert!(ticks.iter().any(|t| t.major));
    }

    #[test]
    fn ticks_go_down_to_milliseconds() {
        // 50 ms over 800 px → 1 ms interval = 16 px < 70 → 5 ms = 80 px
        let ticks = ruler_ticks(0.0, 0.05, 800.0, 70.0);
        assert!(ticks.iter().any(|t| t.label == "5 ms"));
    }

    #[test]
    fn ticks_minutes_labels() {
        let ticks = ruler_ticks(0.0, 300.0, 3000.0, 70.0);
        // 5 s interval over 3000 px = 50 px < 70 → 5.0 is max of NICE →
        // falls back to the coarsest interval
        assert!(!ticks.is_empty());
    }

    #[test]
    fn zoom_span_clamps_and_anchors() {
        let dur = 30.0;
        let full = ViewSpan::full(dur);
        // zoom in around the centre by 0.5 → span 15 s, start 7.5
        let z = full.zoomed(0.5, 0.5, dur, 48_000);
        assert!((z.start - 7.5).abs() < 1e-9);
        assert!((z.end - 22.5).abs() < 1e-9);
        // zoom out at the left edge: the anchor stays the left edge, the
        // span clamps to what fits → [7.5, 30]
        let z2 = z.zoomed(0.0, 100.0, dur, 48_000);
        assert!((z2.start - 7.5).abs() < 1e-6);
        assert!((z2.end - dur).abs() < 1e-6);
        // zoom out around the centre: clamps to the full file
        let z3 = z.zoomed(0.5, 100.0, dur, 48_000);
        assert!((z3.start - 0.0).abs() < 1e-6);
        assert!((z3.end - dur).abs() < 1e-6);
        // never below the 8-sample minimum
        let mut tiny = full;
        for _ in 0..60 {
            tiny = tiny.zoomed(0.5, 0.1, dur, 192_000);
        }
        assert!(tiny.end - tiny.start >= 8.0 / 192_000.0 - 1e-9);
    }

    #[test]
    fn sample_level_detection() {
        let sp = ViewSpan {
            start: 1.0,
            end: 1.0 + 16.0 / 48_000.0,
        };
        assert!(sp.sample_level(48_000));
        assert!(!ViewSpan::full(10.0).sample_level(48_000));
    }
}
