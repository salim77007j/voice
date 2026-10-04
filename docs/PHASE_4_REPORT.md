# Phase 4 Report — Slint UI, wired to the live engine

**Status: COMPLETE** · Date: 2026-10-04 · All tests green (154 + 7 UI), clippy `-D warnings` clean, release binary 27 MB (< 50 MB budget)

Phase 4 scope (ARCHITECTURE_PLAN §14): *"Slint UI: toolbar, waveform canvas with zoom, three precision modules, status bar, EN/AR + RTL, everything wired to the live engine (no fake UI)."* — delivered in full, with pixel-level evidence.

---

## 1. What was built

### 1.1 The window (ui/app-window.slint, §8.2 layout)

- **Toolbar**: app identity (teal radial dot), *New Recording / Stop Recording* (danger-red while capturing), *Import File*, *Export* (primary teal). Vector icons drawn as `Path` elements — no icon font, no emoji, themeable.
- **Transport bar**: rewind / play-pause / stop, zoom in / out / fit, timecode readout (`HH:MM:SS.mmm`, IBM Plex Mono), rate/channels. **Intentionally LTR under Arabic** — universal media convention, documented here as a deliberate design decision.
- **Waveform canvas**: peak image + playhead (2 px teal + glow) + selection (teal @18 %) + hover crosshair; click/scrub to seek, wheel zoom, shift-drag to select. The timeline itself stays LTR in RTL locales (media convention).
- **Right panel** (mirrors to left under RTL): the three precision modules (§8.3).
- **Status bar**: state dot (red = recording, green = playing, amber = paused), composed status line, live preview-latency telemetry, `EN | عربي` language switcher.
- **Keyboard**: Space = play/pause (window-level FocusScope), full tab/arrow operability on the custom widgets.

### 1.2 The three precision modules (§8.3 — 1-cent / 0.1-dB / 1-mm precision)

Each `ModuleCard` carries: icon + title, live mono value, detail readout, the **custom `PrecisionSlider`**, an **editable numeric field**, and a per-module extra:

| Module | Range / step | Detail line | Extra |
|---|---|---|---|
| Pitch | ±12 st, arrows 0.01 st (1 cent), Shift 0.1 st | note + cents (`C 5 +7¢`, A4/MIDI numbering) | — |
| Air & Breath | ±100 %, arrows 0.5 % | `applied −24.0 dB` from **engine telemetry** (or `bypass (neutral)`) | live meter of applied duck gain |
| Formant | 100–260 mm, arrows 1 mm, Shift 10 mm | interval (`interval +2.17 st`) | vocal-tract glyph whose tube length tracks the value |

`PrecisionSlider` (custom Slint component): centre detent tick (lit at neutral), fill from neutral toward the value, thumb with drag glow, **double-click = reset**, Home = neutral, End = bound, arrow/Shift-arrow stepping, **RTL-aware** mapping (screen fraction mirrors; Left/Right swap meaning), 2-px focus ring, `accessible-role: slider`. Value fields parse leniently (`+3`, `-.5`, `3.`) and clamp (committing `99` yields `+12.00`); invalid input restores the canonical text from Rust.

### 1.3 Waveform rendering (§8.4 — O(pixels) at every zoom)

- `PeakMipmap`: ×4 min/max/RMS pyramid over a mono mix, computed once per load; a single loud sample survives **every** level (regression-tested).
- Rendering selects the level whose block density ≥ 1 block/pixel, then draws per-column RMS body (teal @55 %) + peak outline. At deep zoom (< 2 samples/column) it switches to a **stem plot** (requirement R8); the status readout shows *sample level*.
- Zoom: wheel/buttons, span clamped to [8 samples, full file], anchored at cursor; *fit* restores the whole take. Time ruler: 1-2-5 nice-interval ladder with mono labels (ms → s → mm:ss), every 5th tick major.
- Playhead/selection/hover are **overlay elements** — 60 fps playback never re-renders the peak image.

### 1.4 Live preview = the export engine (§6.6, no-fake-UI rule)

`PreviewPlayer` (mvl-app/src/preview.rs): feeder thread owns per-channel `VocalEngine`s (Preview profile, 512/128) constructed **inside** the thread (engines are not `Send`), pushes processed audio into an rtrb ring; the cpal callback fills device buffers with generation-flushed seek logic (same architecture as the Phase 2 player).

- **Neutral parameters ⇒ bit-exact bypass** (engine invariant #1 holds on the live path — tested).
- Slider changes publish into a params slot picked up at the next block (< 3 ms at hop granularity, §8.3).
- Position via lock-free counters; seek/stop rebuild the engines so state never leaks across jumps — **seek-equivalence is regression-tested** against a fresh pipeline.
- Honest telemetry: `applied_air_db` (deepest duck gain, live meter), guard-engaged flag, and the algorithmic latency shown in the status bar (`preview 10.7 ms` at 48 kHz).

### 1.5 Session model (§9.2 — disk-backed recordings)

- **Import** (WAV/MP3, native dialog): decodes on a worker thread; ≤ 48 kHz material stays in RAM, higher rates get a 48 kHz preview copy for the realtime path while the original stays the export source.
- **Record**: 192 kHz/32-f float capture straight to disk (Phase 2 recorder); on stop, the 48 kHz preview is streamed out via the disk-based resampler and the mipmap is built — the full-rate take **never enters RAM**.
- **Export**: native save dialog; in-memory sources render through the exact offline path of the Phase 3 fixtures; on-disk 192 kHz sessions **stream** chunk-by-chunk through Render-profile engines into the output WAV (RAM stays O(chunk)). Progress + realtime factor reported in the status bar. Neutral export of neutral material is bit-exact (tested).

### 1.6 Localization & RTL (§8.6 — with a mechanism correction)

- All UI strings flow through `@tr()`; the Arabic catalog ships at `translations/ar/LC_MESSAGES/mvl-app.po` (gettext format) and is **compiled into the binary**; switching is instant via `slint::select_bundled_translation`.
- **Mechanism correction vs the plan**: Slint 1.18 uses **gettext `.po` bundles**, not Fluent `.ftl` (the Phase 1 research assumption). Capability is identical (source-language default + one catalog + runtime switch); the deviation is documented here and in the worklog.
- **RTL**: no global `layout-direction` exists in Slint 1.18; mirroring is done with `FlexboxLayout { flex-direction: rtl ? row-reverse : row }` on the toolbar, main row and status bar (verified against the compiler source first). Text renders through Slint's bidi pipeline with **IBM Plex Sans Arabic** auto-selected when `locale = "ar"`. Digits stay Western (§8.6).

### 1.7 Typography (§8.7)

IBM Plex Sans (400/500/600) + IBM Plex Sans Arabic (400/500/600) + IBM Plex Mono (400/500) embedded in the binary. The Sans statics were instanced from Google Fonts' variable font with fontTools (IBM's npm/CDN statics were unavailable); family names and weights verified via the TTF name tables. OFL text ships alongside (`assets/fonts/OFL.txt`).

### 1.8 The `screenshot` subcommand (evidence, not mockups)

`micro-vocal-lab screenshot out.png [file] [--pitch/--air/--tract/--locale/--playhead/--width/--height]` renders **the same component tree** through the software renderer offscreen (a custom `slint::platform::Platform` over `MinimalSoftwareWindow`). This is the strongest anti-fake-UI tool: every screenshot in `docs/phase4-screenshots/` is a pixel-true render of the real UI code.

---

## 2. Evidence

| Artifact | What it proves |
|---|---|
| `docs/phase4-screenshots/en-empty.png` | empty state: toolbar, empty hint, three neutral modules, status line |
| `docs/phase4-screenshots/en-neutral-mixed.png` | loaded take, waveform + ruler + playhead, pitch +3.00 st / air −30 % (= −12.0 dB readout) / tract 140 mm with interval |
| `docs/phase4-screenshots/en-zoomed.png` | alternate params (−5 st / +50 % / 210 mm) |
| `docs/phase4-screenshots/ar-rtl-mixed.png` | **Arabic + RTL**: toolbar/status mirrored (export button at left, identity dot at right), side panel on the left, waveform canvas on the right, Arabic labels, RTL slider mapping |

Pixel-level verification (measured, not eyeballed): the EN waveform spans x 0–948 with the panel right; the AR render spans x 304–1248 with the panel left; the mid-canvas sample is the exact 55 % teal-over-panel blend (36, 129, 120); pure `#2DD4BF` peak pixels and the `#1C1E24` panel colour are present in every render.

**Headless integration tests** (`crates/mvl-app/tests/ui.rs`, 7 tests): initial state; session load populates duration/rate/ticks/wave image; param changes sanitize + clamp + restore on bad text (plan §8.3's −12.0 dB = −30 % example asserted); zoom reaches sample level and fit restores; language switch flips the RTL global; play without an output device surfaces the honest no-device status; screenshots render with the design tokens at the right pixels.

**Module tests** (33): timecode/note-name/parse formats; mipmap level shapes, peak survival, stereo mix; render determinism + stem plot; ruler tick spacing/labels; zoom clamps/anchors; preview bypass bit-exactness, mid-stream param change, seek equivalence, sink flush; session shapes, 96 kHz→48 kHz preview, WAV/MP3 export round-trips, streamed on-disk render bit-exactness.

---

## 3. Engineering notes & honest gaps

1. **Fonts via instancing**: IBM Plex Sans statics were unavailable from IBM's own channels during this phase; the committed statics are fontTools instantiations of Google Fonts' variable TTF (wght 400/500/600, wdth 100). If IBM later republishes statics, swapping the files is a drop-in change.
2. **FlexboxLayout stretch**: main-axis growth requires an explicit `alignment: LayoutAlignment.stretch` in Slint 1.18 (unlike `HorizontalLayout`, whose default is stretch). Cost an afternoon of pixel forensics; recorded here for Phase 5.
3. **Slint's context is thread-local**: the headless platform must be installed **per thread** (the test harness spawns a thread per test). `headless::install()` handles this; documented in the module docs.
4. **Waveform render bug found by the tests**: `draw_column` cast a negative row index to `usize` at canvas heights < 5 px, producing a near-infinite loop (the headless window starts at 0×0). Fixed with i64 clamps everywhere + a guard; regression covered by the UI tests that previously hung.
5. **Mipmap aggregation bug found by the tests**: level ≥ 2 min/max were derived from the previous level's RMS column, silently losing peaks. Fixed to min-of-min / max-of-max; the "single loud sample survives every level" test now holds.
6. **Engine `Send`-ness**: `VocalEngine` (rustfft plans, rubato `Async` scratch) is not `Send`; the preview feeder constructs its engines inside its own thread. This also protects the audio path from cross-thread surprises.
7. **RAM model**: imports of very long high-rate files (e.g. > 15 min at 192 kHz) still decode into RAM for the export source (recording sessions do not — they stay on disk). Full disk-streaming import is a Phase 5 optimization if real usage demands it.
8. **rfd/xdg-portal**: file dialogs go through the XDG portal on Linux; in this container there is no portal, so the *dialog* path is exercised on real desktops (Phase 5) — the CLI (`run FILE`, `screenshot FILE`) and every code path behind the dialogs are covered by tests.
9. **Binary size**: 27 MB (budget < 50 MB). Above the §9 estimate (10–18 MB) mainly due to Slint's default image-decoder stack (resvg/webp/jpeg/gif) and accessibility + portal D-Bus machinery. A size pass (feature-trimming `i-slint-core`'s image formats) is queued for Phase 5.
10. **Dev profile**: workspace `dev`/`test` profiles now use `debug = "line-tables-only"` + `incremental = false` — full debuginfo for the Slint tree exceeded the container disk; backtraces keep line numbers.

---

## 4. Verification summary

| Check | Result |
|---|---|
| `cargo test --workspace` | **154 passed, 0 failed** (33 app-lib + 7 headless UI + 69 core + 45 io), 5 ignored (2 hw audio, 2 fixture regenerators, 1 hw preview) |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean |
| `cargo fmt --all` | clean |
| Release build | OK, 27 MB, startup self-check renders, screenshot subcommand verified in release mode |
| EN/AR screenshots | 4 renders committed under `docs/phase4-screenshots/` with pixel-level mirroring verification |
| Hardware audio | None in container (no sound card); all device-touching paths degrade to honest status messages; on-device verification is Phase 5 scope per protocol |

---

## 5. Next (Phase 5 per §14)

CI matrix + artifacts, full test suite green in CI, on-device verification with screenshots (real display, real audio I/O, real dialogs), binary-size pass.
