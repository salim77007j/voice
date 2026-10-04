# Phase 7.2 Report — Studio-Rack UI Redesign (Audioprecise Pro reference)

**Scope:** Replace the v1.0.0 "student-project" UI with a professional
studio-rack interface modeled on the user-supplied reference
(`reference.png`, analyzed via VLM into a full design spec), without
losing a single working interaction — every callback, i18n string, RTL
mirroring rule and accessibility behavior carries over.

**Result:** full visual redesign + a new live analysis rack (RTA spectrum
analyzer, LED master meters, clip latching) driven by a new pure-Rust
analyzer in `mvl-core`. 196 tests green (189 → 196, +7); clippy
`-D warnings` clean; fmt clean. VLM design-review score went from
4/10 (first draft) to **8/10** (professional hardware feel) across three
iterated critique rounds; RTL/Arabic verified pixel-correct.

---

## Design language (from the reference analysis)

| Token | v1.0.0 | v1.1.0 (rack) |
|---|---|---|
| App background | `#141519` | `#1A1D21` deep charcoal |
| Panel | `#1C1E24` | `#242729` dark gunmetal + bevel edges |
| Display wells | — (flat) | `#131519` recessed, inner-shadow rim |
| Borders | `#2E323C` | `#3A3F45` + `#4A5056` machined bevel |
| Accent | teal `#2DD4BF` (everywhere) | **gold `#E6B800`** (vocal/brand/engaged only) |
| Audio data | teal | **blue family**: waveform `#4A90E2`, spectrum `#2C7BE5` |
| Timecode | plain text | green-tinted `#8CF5A0`, 19 px bold mono |
| Corners | 6/10 px | 3/4 px machined |
| Meters | none | segmented LEDs, green→yellow→red, peak hold, clip |

Two-hue discipline replaces the v1.0.0 teal overload: **gold = controls,
blue = audio data**, exactly the reference's channel-color/data-color split
adapted to a single-instrument app.

## New layout (rack architecture)

```
┌ nameplate ─ ● MICRO VOCAL LAB · PROJECT take-one [Import][Export][Devices] ┐  52 px, screws
├ WAVEFORM · OVERVIEW — full-width recessed well, blue trace, gold playhead ─┤  stretch
├ [ PITCH | AIR & BREATH | FORMANT channel strips ]  [ RTA + MASTER LEVEL ] ─┤  292 px
│   knob + fader + value well + LED         40-band analyzer + LED meters   │
├ transport ─ ⏮ ▶(44px) ⏹ ●(44px) 00:00:02.400 / 00:00:06.000   [−][+][fit] ┤  60 px, screws
└ status ─ ● take-one · 48000 Hz · 1 ch · … · EN | عربي ────────────────────┘  26 px
```

The three modules became **vertical channel strips** (header LED +
engraved title, recessed value well in 19 px mono, skeuomorphic rotary
knob, precision fader with metallic cap, telemetry line), matching the
reference's channel-strip anatomy instead of stacked horizontal cards.

## New capabilities (not just paint)

1. **`mvl-core::spectrum` — display analyzer** (pure, 7 unit tests):
   2048-point realfft with Hann window → 40 log-spaced bands
   (20 Hz–20 kHz, stable across 44.1/48/96/192 kHz — tested), peak/RMS
   dBFS, dB→meter mapping. Never panics on empty/short/non-finite/absurd
   input (fuzz-tested).
2. **Live RTA analyzer** in the UI: 30 fps analysis of the preview mix
   centered on the playhead — dances during playback, freezes when
   paused like a parked tape machine, decays to dark with no session.
   Blue gradient bars + white peak-hold caps (hold-then-decay) + engraved
   100/1k/10k frequency rules.
3. **LED master meters** (L/R): 24 segments, green→yellow→red over
   −60…0 dBFS, white peak-hold line, latching CLIP LED (1 s hold).
4. **Skeuomorphic knob widget** (`knob.slint`): beveled rim with seat
   shadow, brushed body, rotating gold indicator with bright tip, 7
   tick marks over the 270° sweep. Drag vertically (~150 px full range,
   Shift = ⅛ fine), wheel = step, arrows = step/coarse, Home = neutral,
   double-click = reset, focus ring. Same propose-only contract as the
   fader — Rust stays the single source of truth.
5. **Honest static renders:** `Controller::refresh()` runs one real UI
   tick (the same code the 30 fps timer runs), so `screenshot`
   subcommand output shows true RTA/meter state, not fake bars.

## Waveform renderer upgrades (Rust)

- Trace recolored to audio-data blue `#4A90E2`.
- RMS body now has an **alpha gradient** (190 at the centerline → 60 at
  the body edge): the analog "weight near the baseline" look from the
  reference, replacing the flat 55 % fill.
- Playhead + selection + glow recolored gold; the canvas sits in a
  recessed well with an inner top shadow.

## Process: three VLM design-review rounds

| Round | Change | Score |
|---|---|---|
| 1 | first full rack draft (empty state) | 4/10 — "skin without hardware details" |
| 2 | + audio loaded | 7/10 — "high-end skin, screws/recesses too subtle, RTA empty" |
| 3 | + screws 9 px & on all racks, recessed waveform well, RMS gradient, 66 px knobs, honest RTA in screenshots, decluttered nameplate | **8/10** — "iZotope/Antares tier" |

Remaining round-3 notes are live-behavior items that already exist but
cannot show in a static PNG (30 fps RTA animation, peak-hold decay,
knob drag) or are inherent to the software renderer (waveform edge
stair-stepping; the femtovg desktop renderer smooths it).

## Verification

- **196/196 tests** (189 → 196): +7 `mvl-core::spectrum` (sine band
  placement, level tracking, white-noise coverage, no-panic fuzz,
  cross-rate band stability, meter mapping), UI pixel-truth test
  rewritten for the new palette + RTA model assertions (40 bands, ≥3 lit
  on voiced material, meter > 0, no false clip).
- **clippy `-D warnings` clean**; **fmt clean**.
- **RTL/Arabic** verified by VLM on rendered pixels: nameplate, strip
  order, analysis panel position and status bar all correctly mirrored;
  Arabic shaping clean (all new strings translated in the .po catalog).
- Evidence: `docs/phase7-evidence/ui-en-v1.1.0.png`,
  `docs/phase7-evidence/ui-ar-v1.1.0.png` (1440×900, real loaded audio,
  real RTA data, non-neutral parameters).

## Files

- **New:** `ui/widgets/rack.slint` (RackPanel/ScrewHead/LedDot/DisplayWell/
  EngravedLabel), `ui/widgets/knob.slint`, `ui/widgets/spectrum-view.slint`,
  `ui/widgets/level-meter.slint`, `crates/mvl-core/src/spectrum.rs`
- **Rewritten:** `ui/app-window.slint` (rack architecture), `ui/theme.slint`
  (industrial palette), `ui/widgets/module-card.slint` (channel strip)
- **Restyled:** `precision-slider.slint` (fader cap; interaction code
  untouched), `buttons.slint`, `waveform-view.slint`, `icons.slint`
- **Extended:** `controller.rs` (RTA state + 30 fps publishing +
  `refresh()`), `main.rs` (screenshot tick), `waveform.rs` (blue +
  gradient RMS), Arabic catalog (+15 strings)
