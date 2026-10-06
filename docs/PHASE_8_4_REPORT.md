# Phase 8.4 Report — Broadband Compressor + Live GR Metering

**Scope:** the second V2 rack module — a real channel compressor (threshold /
ratio / attack / release / knee / makeup / mix), every control wired 1:1 to
the new feed-forward DSP core, with an honest live gain-reduction meter, a
controller-computed static transfer curve, five presets and full Arabic RTL.

**Acceptance (roadmap §8.7 row, dynamics part):** *compressor GR meter within
±0.5 dB of the analytic envelope* — covered exactly (see §4.1); *panels real
(no fake UI — pixel-verified)* — the transfer curve is computed by the same
`static_gain_db` math the audio path runs, the GR meter is engine telemetry,
and the evidence renders carry a GR value produced by an offline engine probe
of the loaded take.

---

## 1. DSP core (`crates/mvl-core/src/compressor.rs`, new; ~840 lines with tests)

- **Topology** — classic feed-forward log-domain program compressor
  (Reiss & McPherson, *DAFX* §6.4): rectify → dB → static gain computer →
  one-pole ballistics on the gain-reduction signal → gain application with
  makeup → linear dry/wet mix. All math in `f64`; the per-sample state is
  three accumulators — no allocation, no locks, RT-safe.
- **Static gain computer** — the piecewise Zölzer (*DAFX* 2nd ed. §4.3.2)
  curve: zero below `T − W/2`, quadratic blend across the `W`-dB soft knee,
  `(x − T)·(1 − 1/R)` above. `R = 1` is the identity everywhere; `W = 0`
  degenerates to the hard knee; NaN-free over the whole documented range.
  Exposed as a pure `static_gain_db()` shared by the processor, the UI
  transfer curve and the tests. *(The first transcription carried a double
  negation in the knee branch — the knee would have boosted. The
  monotonicity + continuity tests caught it before any audio ran; §4.1.)*
- **Ballistics** — one-pole smoothing in the dB domain with e-folding time
  constants: `coef = exp(−1/(τ·fs))`, redesigned lazily only when the
  `(attack, release, rate)` triple changes. After `τ` the envelope has
  covered exactly `1 − 1/e` of the step — a testable identity, not an
  approximation (§4.1). Attack deepens, release recovers.
- **Detector** — sample-absolute peak with a −120 dBFS floor. Peak (not RMS)
  keeps transients predictable and makes the steady-state identity exact.
- **Ranges** — threshold −60…0 dBFS, ratio 1…20 (`1.0` = inert), attack
  0.1…200 ms, release 5…2000 ms, knee 0…24 dB, makeup −24…+24 dB (wet path),
  mix 0…100 % (0 % = dry-only, inert). Gain reduction clamped at −80 dB.
- **`CompressorParams`** (plain `Copy`) with `sanitized()` (clamps +
  non-finite fallbacks) and `is_active()` = enabled ∧ mix > 0 ∧ (ratio > 1 ∨
  makeup ≠ 0) — the signal-independent inertness decision the engine's
  bit-exact bypass uses. A −0.0 sentinel survives an inactive module.
- **Presets** — `Gentle` (glue, wide knee), `VocalControl` (3.5:1 @ −20 dBFS,
  +2.5 dB makeup), `PeakTamer` (hard-knee 6:1, 0.5 ms attack), `Broadcast`
  (denser, always-on, +4 dB), `NyParallel` (8:1 crushed wet blended at
  30 %) — documented musical intents, display order = ComboBox order.
- **Metering** — `process()` returns the deepest GR of the call (dB, ≤ 0) at
  block cadence; `transfer_pairs_db()` serves the UI the −80…0 dB static
  curve.

## 2. Engine integration (`crates/mvl-core/src/engine.rs`)

`VocalParams` gained `comp: CompressorParams` (struct ≈ 28 bytes larger,
still plain `Copy` on the same parameter channel — never the audio
callback). The chain order is now:

```text
vocal chain (STFT) → true-peak guard → EQ (8.3) → compressor (8.4)
```

The compressor is the **last** module: it shapes the dynamics of the
already-toned signal, and its makeup gain can exceed the ceiling exactly
like the EQ's boosts can (the offline source-relative bound in `render`
still sees the final signal). Bypass rules:

- whole struct neutral (vocal sliders neutral ∧ EQ inert ∧ comp inert) →
  bit-exact passthrough, invariant #1 unchanged — and the comp's envelope
  resets so a later re-engage starts from a fresh state (no stale gain jump
  at the seam);
- vocal sliders neutral with post-chain modules active → **post-only path**:
  the STFT machinery stays *off*; the EQ and/or compressor run per-sample
  (instant A/B preserved, deactivating restores the instant bypass);
- vocal chain engaged → chain output → guard → EQ → comp.

`flush` runs the same stages on the tail — the release stays in force and
the stream total remains exactly the input length. Metering: the engine
records the deepest GR of the most recent block (`current_gr_db()`, live
value that recovers with the music and freezes on pause) plus a cumulative
deepest (`RenderResult.applied_gr_db`) for offline evidence. Preview, both
export paths, the `process` CLI and the selftest inherit the compressor
with zero plumbing — what you hear is what you export.

## 3. UI (`crates/mvl-app/ui/widgets/comp-panel.slint`, new) + wiring

- Full-width rack row (196 px) directly under the EQ row (signal-flow
  order): header (status LED · COMPRESSOR · ON/BYPASS master toggle · preset
  ComboBox · Reset) over a body of three zones — the **transfer-curve well**
  (260×260 viewbox, both axes −80…0 dBFS: unity diagonal for reference, the
  controller-computed static curve + fill, a threshold rule at its axis
  fraction, LTR in RTL locales per the engineering-graph convention), the
  **GR meter well** (0…−24 dB vertical fill from live engine telemetry, mid
  rule at −12 dB, numeric readout) and the **knob strip**: threshold (linear)
  · ratio (log) · attack (log) · release (log) · knee · makeup · mix, each
  with a canonical mono readout, plus a "Gain reduction −x.x dB" /
  "Idle" / "Bypassed" state line.
- **No fake UI:** every knob proposes through `comp-param-changed(param,
  value)`; the controller mutates `Inner.params.comp`, re-sanitizes, pushes
  to the preview player and republishes canonical values + curve + texts.
  The ComboBox highlight is exact preset equality; custom curves keep the
  previous highlight (documented cosmetic, same as the EQ).
- **Live GR flow:** feeder block → `Shared.gr_db_bits` (atomic) →
  `PreviewPlayer::current_gr_db()` → `ui_tick` publishes value + text at the
  30 fps tick, mirroring the air meter's precedent.
- **Arabic RTL:** all strings through `@tr` with 19 new `ar` catalog entries
  (الضاغِط، العتبة، النسبة، الهجوم، التحرر، الركبة، التعويض، المزج، خفض
  الكسب، … and the five preset names); the panel mirrors via the
  `RowReverse` convention, the transfer curve's dB axes stay LTR (same
  decision as the EQ response, RTA and timeline).
- `screenshot`/`process` CLIs gained `--comp-preset
  gentle|vocal|tamer|broadcast|ny`. The screenshot path runs an **offline
  GR probe**: with the comp active and a take loaded it renders the take
  through the real engine once and publishes the deepest GR, so static
  evidence shows a meter value the engine actually produced.
- `selftest` evidence renders and the `ui.rs` pixel test moved to
  1280×1060 (two 196 px post-chain rows no longer fit 800 px; the pixel
  probes were recomputed: waveform mid y 159, status strip y 1049).

## 4. Verification

### 4.1 New tests (17)

`mvl-core::compressor` (12): static hard-knee hand math (below/at/above,
20:1 → 9.5 dB, ratio-1 identity); soft-knee continuity at both knee edges +
closed-form centre value + monotone deepening — *these caught the double
negation in the knee branch (a boost instead of a cut) before any audio
ran*; **the acceptance test**: steady-state measured GR == analytic static
curve ≤ 0.5 dB over 7 cases spanning hard/soft knees, ratios 2–20 and an
in-knee drive (constant-level stimulus — a rectified sine sweeps the whole
dB trajectory twice per cycle and the asymmetric ballistics ratchet, so it
is the wrong probe for a static identity; documented in the test); attack /
release e-folding identity (63.2 % of the step at τ, ±2 %, per-sample
`out/in` gain on constant-level steps); rate-change redesign (τ identity
holds at 44.1/48/96 kHz); bit-exactness of disabled/ratio-1/mix-0 params
including a −0.0 sentinel and denormal-scale samples; deterministic 50 %
mix crossfade; exact wet-path makeup below threshold; full-range
NaN/Jury-style sweep (1 458 param corners × noise/impulse/DC/silence, GR
bounded in [−80, 0]); reset-clears-envelope; transfer-curve grid + endpoint
+ monotone-reduction checks.

`mvl-core::params` (1): comp participates in neutrality + sanitization
(idempotent clamps). `mvl-core::engine` (3): comp-only path == bare
processor + instant bypass restoration + fresh envelope; comp runs **after**
the EQ (engine output == manual EQ→comp); engaged-chain invariants (length
exactness, `applied_gr_db` ≤ −1 dB on a squeezed render, bit-exact bypass
with knobs parked but master off).

`mvl-app::format` (1): canonical comp texts (`-20.0 dB`, `3.5:1`, `10.0 ms`,
`+2.5 dB`, `100 %`, GR never reads positive).

### 4.2 Suite health

- `cargo test --workspace` (with `MVL_VIRTUAL_AUDIO=1` virtual-device
  smokes): **264 passed / 0 failed / 3 ignored** — 247 baseline + 17 new,
  zero regressions.
- `cargo fmt --all -- --check` clean; `cargo clippy --workspace
  --all-targets -- -D warnings` clean.

### 4.3 Evidence renders (`docs/phase8-evidence/`)

- `comp-panel-en.png` — Vocal Control preset on
  `testdata/voiced_harmonic_stack_220hz.wav`: transfer curve with the knee
  bend against the unity diagonal, threshold rule at −20 dBFS, GR meter
  filled to **−7.8 dB** — the offline probe's real measurement — and the
  readout row `−20.0 dB · 3.5:1 · 8.0 ms · 150.0 ms · 6.0 dB · +2.5 dB ·
  100 %` with "Gain reduction −7.8 dB".
- `comp-panel-ar.png` — Broadcast preset under RTL: mirrored header
  (الضاغِط / تشغيل / إعداد مسبق «بث» / إعادة تعيين), mirrored knob strip
  (العتبة −26.0 dB … المزج 100 %), curve + GR well on the right, the probe's
  −13.5 dB fill, Arabic state line (خفض الكسب) — Western digits/units per
  the localization conventions.

## 5. Honest gaps

1. **Block-rate metering.** The GR meter updates at feeder-block cadence
   (~43 ms) — smooth enough for the 30 fps UI tick, but it shows the
   *deepest* reduction of the block, not an instantaneous trace. A
   per-hop ring of GR values (like the RTA's snapshot path) is the 8.5
   polish if listening tests want oscilloscope-style traces.
2. **No sidechain filter.** The detector is broadband; a high-passed
   sidechain (de-mud-friendly) is the classic next control and pairs
   naturally with the de-esser phase.
3. **`matching_comp_preset_index` fallback.** A custom curve leaves the
   ComboBox at its previous index (same cosmetic as the EQ's); a "Custom"
   placeholder row is the shared 8.5 polish.
4. **Static curve view spans −80…0 dBFS** while thresholds reach −60 dB —
   the threshold rule and knee always fit the view; the curve can never
   clip (deliberate headroom, mirroring the EQ's ±26 dB view of ±24 dB
   bands).
5. **Ballistics convention.** τ is the e-folding constant, not the
   10–90 % rise time some datasheets quote (factor ln(9/… )≈2.2 between
   them); the doc comment and tests pin the convention exactly.

## 6. Exit state

Phase 8.4 complete and pushed: compressor DSP + engine integration (last in
chain, after the EQ) + full UI (EN/AR) + live GR meter + offline GR probe +
17 tests, `cargo test --workspace` 264/264, fmt + clippy clean. Waiting for
"continue" → Phase 8.5.
