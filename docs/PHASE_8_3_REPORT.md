# Phase 8.3 Report — 4-Band Parametric EQ Panel

**Scope:** the first V2 rack module — a real channel EQ (Low Shelf / Low-Mid
Bell / High-Mid Bell / High Shelf), every control wired 1:1 to real biquad
DSP, with a live response curve, per-band and master bypass, five presets
and full Arabic RTL. Plus two CI deliverables requested alongside: per-job
Rust cache keys, and a hotfix for the Phase 8.2 macOS e2e flake that CI
caught on both macOS legs.

Commits this phase: `4c8e4da` (CI cache keys) → `7fc2a1d` (macOS e2e
hotfix) → this one (EQ + docs).

---

## 1. CI: per-job rust-cache keys (Request 1)

`Swatinem/rust-cache@v2` was already present in **all** Rust-compiling jobs
since Phase 5.1 (lint, the 5-OS test matrix, the 4-OS release matrix) — the
rebuild-from-scratch scenario the request describes has not existed here for
five phases. What was missing was the **per-job `key` partition**:

- `lint` — debug profile + clippy artifacts (`key: lint`)
- `test` — debug profile, one cache per matrix OS (rust-cache already
  partitions by runner OS + rustc version; `key: test` keeps it separate
  from lint's clippy-fingerprinted target dir)
- `release` — release profile (`key: release`), which previously thrashed
  the debug jobs' restored target dirs on the same OS

**Observed behavior:** the first run with the new keys was an expected
MISS per key; the very next run restored `v0-rust-lint-lint-Linux-x64-…`
with `full match: true` (`Cache restored successfully`). Full CI run
(10 jobs: lint + 5 test legs + 4 release legs) green — see §3.

No `nightly`/`asan` jobs exist in this repo's CI; the rule "every job that
compiles Rust carries rust-cache" is satisfied by all three job families.

## 2. Phase 8.2 follow-up: macOS e2e flake (hotfix `7fc2a1d`)

The Phase 8.2 acceptance test `e2e_playhead_matches_audible_audio_within_5ms`
failed on **both macOS CI legs** (reported 24.45 ms ahead of audible) while
passing on Linux/Windows. Root cause was the **test harness, not the
player**:

- the simulated device reported a *constant* 21 ms latency regardless of
  its pipeline state, and the expected value was a *wall-clock* formula;
- on a loaded runner the probe thread is descheduled at startup, then the
  absolute-target schedule fires callbacks back-to-back (CI log: 6 probes
  covered 25.6 ms instead of 50 ms) and the ring fed short once (5 of 6
  blocks handed);
- a real device cannot hand 50 ms of content in 25 ms of wall time — and
  if it ever bursts, its host-reported latency (queue depth, exactly what
  ALSA delay / WASAPI padding / CoreAudio report through cpal) *grows*
  with the burst. The player's anchor math is exact given per-callback
  queue depth; the sim's constant 21 ms was the unphysical part.

**Fix:** the simulated device now models its pipeline honestly — a queue
seeded with 21 ms of pre-roll that drains in real time, each callback
reporting its actual pre-block queue depth as the callback→audible latency.
Player anchor vs sim truth is then algebraically exact between events,
including bursts, pipeline drains and underruns (the handed-cursor clamp
converges both at full drain). The deterministic schedule deliberately
stresses all three regimes (steady 10 ms cadence, a 3-probe 1 ms burst,
a 30 ms drain), and the 5 ms acceptance now holds under **any** scheduling.
`playhead_uses_device_rate_after_resample` was de-flaked the same way (its
assertion cancels wall time since the last callback instead of assuming an
ideal cadence; still catches the frames-vs-seconds bug class). Local stress:
10× consecutive green.

## 3. The EQ panel

### 3.1 DSP core (`crates/mvl-core/src/eq.rs`, new; 1,053 lines with tests)

- **Biquads per the Audio EQ Cookbook** (Robert Bristow-Johnson): band
  kinds fixed by position — 0 low shelf, 1/2 bells, 3 high shelf. All four
  bands share `α = sin ω₀ / 2Q` with `A = 10^(gain/40)`; the cookbook's
  shelf-slope `S` form was rejected deliberately (it demands NaN guards
  for `S > 1` at large boosts; the shared-α convention is NaN-free across
  the full stated range and makes Q behave consistently: higher = tighter
  knee, on shelves too). Coefficients computed and held in `f64`, TDF2
  difference equation (two state accumulators per band — RT-safe, no
  allocation, no locks). Frequencies are clamped under `0.45·Fs` at design
  time (a 20 kHz band on a 44.1 kHz stream lands at 19.845 kHz — stable
  and honest).
- **`EqParams`** (master enable + 4 × `EqBandParams{freq, q, gain_db,
  enabled}`), plain `Copy`, ranges 20 Hz–20 kHz / 0.1–10 / ±24 dB enforced
  by `sanitized()` with non-finite fallbacks. `is_active()` = master on
  **and** any band (enabled ∧ gain ≠ 0) — a zero-gain band's RBJ
  coefficients are exactly the identity, so inert bands are *skipped*,
  which is both a CPU saving and the bit-exactness guarantee (no
  `−0.0 → +0.0` sign flips from identity filters).
- **`EqProcessor`** — four sections in series, coefficients redesigned
  lazily only when a band's `(freq, q, gain)` triple changes (block rate),
  plus `response_db` (summed analytic |H| of active bands) and
  `response_curve_db` (129 log-spaced points, 20 Hz…20 kHz) for the UI.
- **Presets** — `EqPreset::{Flat, VocalPresence, DeMud, AirBoost,
  DeHarsh}` with documented musical intent (e.g. Vocal Presence: −3 dB
  shelf @ 100 Hz, −2 dB bell @ 300 Hz, +2.5 dB bell @ 3.5 kHz, +3 dB
  shelf @ 9 kHz).

### 3.2 Engine integration (`crates/mvl-core/src/engine.rs`)

`VocalParams` gained `eq: EqParams` (struct ≈ 72 bytes larger, still
plain `Copy` — it travels the same parameter channel as before, never the
audio callback; the strategy risk table's `#[repr(C)]` versioning only
becomes necessary if params ever move through raw atomics). The engine
applies the EQ **after** the true-peak guard in both `process` and
`flush` — the last module in the chain, so an offline source-relative
guard still sees the boosted signal in `render`. Bypass rules:

- whole struct neutral (vocal sliders neutral **and** EQ inert) →
  bit-exact passthrough (invariant #1, unchanged);
- vocal sliders neutral with only the EQ active → **EQ-only path**: the
  biquads run while the STFT machinery stays *off* (no engagement, no OLA
  state — deactivating the EQ restores the instant bit-exact bypass, and
  the output is sample-identical to a bare `EqProcessor`);
- vocal chain engaged → chain output passes through the EQ on the way out.

Because the EQ lives inside `VocalParams`, preview, export (both in-memory
and streaming WAV paths), the `process` CLI and the selftest all inherit
it with zero plumbing changes — what you hear is what you export, by
construction.

### 3.3 UI (`crates/mvl-app/ui/widgets/eq-panel.slint`, new)

- Full-width rack row (196 px) between the waveform rack and the channel
  strips: header (status LED · EQUALIZER · ON/BYPASS master toggle ·
  preset ComboBox · Reset) over a body of the response-curve well plus
  four band strips.
- **Response curve:** the controller computes the 129-point summed
  magnitude response with the *same* RBJ code that processes audio and
  publishes it as two path-command strings (open stroked line + closed
  fill area, viewbox 1000×260, ±26 dB full scale) rendered with
  `ImageFit.fill` (Slint's default `contain` letterboxes the fixed-ratio
  viewbox — found and fixed during evidence rendering). Gold when
  engaged/active, muted grey when bypassed; grid rules at +24/0/−24 dB
  and 100 Hz/1 kHz/10 kHz with mono captions.
- **Band strips:** bypass LED button (whole strip header), three knobs —
  frequency and Q sweep **logarithmically** (new `logarithmic` Knob mode:
  drag/wheel/keys all move in fraction space; linear knobs behave exactly
  as before), gain linear — and a mono readout (`100 Hz  −3.0 dB`).
  Disabled strips dim to 55 % opacity; a disabled master dims everything.
- **No fake UI:** every knob, toggle and the preset box propose through
  callbacks; the controller mutates `Inner.params.eq`, re-sanitizes,
  pushes to the preview player (`set_params`) and republishes canonical
  values + curve + texts. The ComboBox highlight is derived by exact
  equality against the preset list (custom curves highlight nothing/Flat).
- **Arabic RTL:** all strings through `@tr` with new `ar` catalog entries
  (المُعادِل، رفّ الترددات المنخفضة، جرس المنخفض المتوسط، … إزالة الكدرة،
  تعزيز الهواء، تنعيم الحدّة); the panel layout mirrors via the existing
  `flex-direction: RowReverse` convention. The frequency axis itself and
  the curve stay LTR — the same engineering-graph decision as the RTA
  spectrum and the timeline. Band titles translate inside Slint (`@tr` in
  a component function keyed by band index); preset labels are a Slint
  `@tr` array; Rust never fabricates translated text.
- Knob logarithmic mode, `enabled` state (grey + inert) and the EQ panel
  are additive; the three existing module cards are untouched.

## 4. Verification

### 4.1 New tests (17)

`mvl-core::eq` (11): **response matches the analytic biquad at 5 test
frequencies per band kind** (f₀/8, f₀/2, f₀, 2f₀, 8f₀ — steady-state sine
measurement vs an independent literal cookbook transcription, ≤ 0.05 dB),
bell peak = requested dB exactly, shelf asymptotes H(1) = H(−1) = A²,
high-Q (Q = 10) accuracy, NaN/stability (Jury) sweep across the full
stated parameter range incl. the Nyquist clamp, bit-exactness of
flat/disabled/all-bands-bypassed EQ (with a `−0.0` sentinel), inert-band
skip ≡ identity, response sum = product of bands, curve grid + endpoints,
sanitization, preset shapes, rate-change redesign (behavioural).
`mvl-core::params` (2): EQ participates in neutrality decisions; EQ is
sanitized together with the sliders (idempotent). `mvl-core::engine` (3):
EQ-only path = bare biquads + instant bypass restoration, EQ applies after
the chain with length invariants, disabled/flat EQ keeps invariant #1
bit-exact. `mvl-app::format` (1): canonical EQ texts.

### 4.2 Suite health

- `cargo test --workspace` (with `MVL_VIRTUAL_AUDIO=1` virtual-device
  smokes): **247 passed / 0 failed / 3 ignored** — 230 baseline + 17 new,
  zero regressions (golden fixtures, import robustness, UI tests all
  green with the enlarged `VocalParams`).
- `cargo fmt --all -- --check` clean; `cargo clippy --workspace
  --all-targets -- -D warnings` clean.
- CI (`7fc2a1d`): all 10 jobs green — lint + test (ubuntu 22.04/24.04,
  windows, macOS 15 arm64 + x86_64) + 4 release artifacts; cache restored
  `full match: true` per job key.

### 4.3 Evidence renders (`docs/phase8-evidence/`)

- `eq-panel-en.png` — Vocal Presence preset: the curve shows the −3 dB
  low shelf, the 300 Hz dip, the 3.5 kHz presence ridge and the 9 kHz air
  shelf; strips read `100 Hz −3.0 dB … 9.00 kHz +3.0 dB`.
- `eq-panel-ar.png` — De-Mud preset under RTL: mirrored header (المُعادِل /
  تشغيل / إعداد مسبق «إزالة الكدرة» / إعادة تعيين), mirrored strips with
  Arabic band titles, the −4 dB @ 250 Hz mud cut visible in the curve
  (LTR axis as designed), Western digits/units per the localization
  conventions.

## 5. Honest gaps

1. **Block-rate coefficient updates.** Sliders re-target coefficients per
   feeder block (~43 ms at 48 kHz preview). Fast drags on narrow-boost
   bands can zipper. The biquad math and tests are ready for sub-block
   crossfade refinement (8.4 candidate) if listening tests demand it;
   clicks are bounded because coefficient jumps at 0 dB crossing are
   identity (bands skip silently when inert).
2. **`matching_preset_index` fallback.** A custom curve that equals no
   preset leaves the ComboBox at index 0 (Flat) rather than a "custom"
   entry — cosmetic, documented; a "Custom" placeholder row is the 8.4
   polish.
3. **Shelf Q semantics.** Shelves reuse the bell's α convention (knee
   sharpness), not a slope-in-octaves control; consistent and NaN-free,
   but users coming from Pro-Q-style slope shelves will find shelf Q
   subtler than bell Q.
4. **Curve view clamps at ±26 dB** while bands reach ±24 dB — the curve
   can never clip the display (deliberate 2 dB headroom), so the drawn
   curve is exact, never compressed.
5. **`q-text` is carried in the UI model but not displayed** (strip width
   budget); the Q knob position is the live indicator. A hover tooltip /
   mini readout is the natural follow-up.

## 6. Exit state

Phase 8.3 complete and pushed: EQ DSP + engine integration + full UI
(EN/AR) + 17 tests + evidence renders + CI cache keys + macOS e2e hotfix,
`cargo test --workspace` 247/247, fmt + clippy clean, CI fully green with
cache hits. Waiting for "continue" → Phase 8.4 (compressor).
