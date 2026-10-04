# Micro-Vocal Lab — Architecture Plan

**Phase 1 deliverable** · Status: APPROVED FOR IMPLEMENTATION · Date: 2026-10-04
Repository: https://github.com/salim77007j/voice · License: GPL-3.0

---

## 1. Vision

Micro-Vocal Lab is a free, lightweight, Rust-native desktop application for **microscopic control over the recorded human voice**. It decomposes a vocal recording into three natural components — **pitch**, **breath/air**, and **formants (vocal-tract shape)** — and lets the user reshape each with professional precision, while strictly preserving natural timing and articulation. It is the free, 2026, Rust-powered answer to Melodyne-class tools: studio-grade quality, native on Windows/macOS/Linux, small binary, low RAM, sub-second startup, English-first UI with optional Arabic RTL localization.

Three non-negotiable quality pillars:

1. **Naturalness** — no chipmunk effect, no robotic/metallic artifacts, no formant warping, no phase smearing.
2. **Precision** — pitch to 1 cent, air/breath to 0.1 dB, formants expressed in millimetres of perceived vocal-tract length.
3. **Surgical integrity** — natural timing and articulation are preserved by design, at the algorithm level, not by approximation.

---

## 2. Requirements traceability

| # | Requirement (from brief) | Architectural answer | Section |
|---|---------------------------|----------------------|---------|
| R1 | Entirely Rust; Win/macOS/Linux native | Rust workspace, Slint + winit, cpal; no web runtime | §3, §7 |
| R2 | English UI default; optional Arabic RTL | Slint Fluent localization + logical layout direction | §8.6 |
| R3 | Small binary, low RAM, fast start | No webview, embedded fonts, disk-backed recording, lazy init | §9 |
| R4 | Record at 192 kHz / 32-bit float | cpal input stream, device-capability negotiation + graceful fallback | §5.1 |
| R5 | Import WAV + MP3 | hound (WAV) + symphonia (MP3, container-probed) | §5.2 |
| R6 | Export WAV + MP3 (configurable bitrate) | hound 32-float WAV; LAME 3.100 (bundled, static) via mp3lame-encoder, CBR 32–320 kbps + VBR presets | §5.3 |
| R7 | Waveform, sub-millisecond zoom | Peak mipmap cache, GPU-composited waveform image, zoom to sample level | §8.4 |
| R8 | Pitch ±12 st, 1-cent, timing preserved | Phase-locked phase vocoder TSM → sinc resample → formant compensation | §6.2 |
| R9 | Air/breath add/remove, 0.1 dB precision | Voicing-aware spectral analysis + time/band-smoothed spectral gain + de-esser + envelope-gated air synthesis | §6.4 |
| R10 | Formant in mm, independent of pitch | True-envelope estimation + Bark-scale envelope warping; 1/L physical mapping vs 170 mm reference | §6.3 |
| R11 | Real-time preview < 20 ms | Dedicated low-latency profile: 512-pt STFT @ 48 kHz (10.7 ms) + 128-smp output buffers | §6.6 |
| R12 | No fake UI | Every control bound to the live DSP parameter bus; bypass path bit-exact and unit-tested | §7.4, §10 |
| R13 | Native look, $200-tool polish | Full design system: IBM Plex family, dark studio palette, custom widgets | §8 |

---

## 3. Technology stack — final decisions

| Layer | Choice | Version (verified 2026-10-04, crates.io) | License | Why |
|---|---|---|---|---|
| UI framework | **Slint** | 1.18.1 (updated 2026-09-21) | GPL-3.0 (free tier) | Declarative, GPU-accelerated, native, RTL-capable, Fluent i18n, tiny binaries. Full analysis in §4 |
| Audio I/O | **cpal** | 0.18.2 (updated 2026-08-16, 22.8M dl) | Apache-2.0/MIT | The de-facto cross-platform audio I/O in Rust; WASAPI/CoreAudio/ALSA backends; arbitrary sample-rate negotiation |
| WAV read/write | **hound** | 3.5.1 (stable since 2023, 19.9M dl) | Apache-2.0/MIT | Mature, complete 32-bit IEEE-float WAV support both directions |
| MP3 + general decode | **symphonia** | 0.6.1 (updated 2026-08-13, 15.7M dl) | MPL-2.0 | Pure-Rust MP1/2/3 decode, format auto-probing; also gives WAV/FLAC/OGG/AAC import for free |
| MP3 encode | **mp3lame-encoder** (LAME 3.100 bundled, statically built) | 0.2.5 (updated 2026-08-20, 1.1M dl) | LGPL-3.0 | Industry-reference LAME encoder, source bundled (verified: `include = ["lame-3.100/**"]`, built via cc/autotools — no system dependency). Configurable CBR + VBR |
| FFT | **rustfft** + **realfft** | 6.4.1 / 3.5.0 | MIT | SIMD-accelerated, pure Rust; realfft gives efficient real-to-complex STFT |
| Resampling | **rubato** | 5.0.1 (updated 2026-10-01, 12.3M dl) | MIT/Apache-2.0 | High-quality sinc/polyphase resampler used inside the timing-preserving pitch path |
| RT-safe queue | **rtrb** | 0.4.0 (12.6M dl) | MIT/Apache-2.0 | Lock-free SPSC ring buffer, purpose-built for real-time audio callbacks |
| F0 tracking | **pitch-detection** (YIN/MPM) | 0.3.0 (72k dl) | MIT | Validated YIN + McLeod pitch method; wrapped by our own voicing/confidence logic (see §6.1) |
| Concurrency | crossbeam-channel, rayon | 0.5.17 / 1.12.0 | MIT/Apache-2.0 | Message passing app↔audio; frame-parallel offline render |
| Errors | thiserror | 2.0.21 | MIT/Apache-2.0 | Typed errors in library crates; no panics on user data |
| Fonts | IBM Plex Sans, IBM Plex Sans Arabic, IBM Plex Mono | latest static | OFL 1.1 | Cohesive superfamily, tabular numerals, full Arabic coverage, embeddable |

**Workspace layout** (implementation begins Phase 2):

```
voice/
├── Cargo.toml                # workspace root
├── crates/
│   ├── mvl-core/             # DSP engine: analysis, pv, envelope, breath, engine (no UI, no I/O deps)
│   ├── mvl-io/               # record (cpal), playback, WAV/MP3 import/export, peak mipmap
│   └── mvl-app/              # Slint UI, transport, threading, export pipeline
├── assets/fonts/             # IBM Plex (embedded via slint-build)
├── testdata/                 # synthetic test signals (committed, deterministic) + CC0 samples
├── docs/                     # this plan, worklog, phase reports
└── .github/workflows/        # CI (Phase 5)
```

Splitting `mvl-core` from `mvl-io`/`mvl-app` is deliberate: the DSP engine must be **pure and unit-testable without audio hardware** (CI runs headless), and the same engine object must be shared verbatim by preview and export — this is the structural guarantee behind the no-fake-UI rule.

---

## 4. UI framework decision: Slint 1.18 (and why not the others)

**Decision: Slint.** Justification against the four criteria that matter for this product:

1. **Professional visual quality at native performance.** Slint's declarative language gives first-class states, animations, gradients, shadows, and scoped styling — the raw material of a "$200 tool" aesthetic — rendered on GPU (FemtoVG/WGPU by default, Skia available) inside a native OS window via winit. No browser, no DOM, no JS bridge.
2. **Right-to-left is actually achievable.** Verified in the Slint source (Phase 1 research, `internal/compiler/builtin_elements.rs`): `FlexboxLayoutDirection` provides `row-reverse` for mirrored item flow, and text alignment supports logical `start`/`end` that resolve by text direction. Combined with Slint's built-in **Fluent localization** (`.ftl` catalogs, `update_all_translations()`), a genuine English/Arabic switcher is a supported pattern, not a hack. We adopt a strict rule from day 1: *every* layout uses logical direction and alignment so the whole UI mirrors when `rtl` is toggled.
3. **Resource envelope.** Slint compiles UI to Rust code at build time (or interprets at dev time); apps ship in single-digit-to-low-tens of MB with instant cold start — squarely inside our < 50 MB / < 200 MB RAM / < 1 s budget. A Tauri equivalent would spend its entire RAM budget on the webview alone.
4. **Maturity and velocity in 2026.** v1.18.1 released 2026-09-21; active "Desktop-Ready" initiative (rich text, keyboard shortcuts, accessibility work); deterministic builds; three renderers with software fallback on Windows OpenGL-less machines.

**Rejected alternatives:**

| Framework | Verdict | Reason |
|---|---|---|
| **egui** (0.36.2) | Rejected | Superb for engineering tools, but immediate-mode redraw model costs idle CPU/battery during animations; default aesthetic needs heavy theming to reach studio grade; **no layout mirroring for RTL** (bidi text ≠ mirrored UI); custom-widget polish (knobs, meters) is more effort than value here |
| **iced** (0.14.0) | Rejected | Clean Elm architecture and MIT license, but no built-in RTL, fewer polished primitives, and the widget set targets generic desktop apps rather than dense studio UIs; slower to a bespoke dark studio look |
| **Tauri** (2.12.1) | Rejected | Best-in-class RTL via CSS and web fonts, but ships a full webview runtime: +10–30 MB binary, 150–400 MB RAM, slower cold start. Directly violates the lightweight mandate; "web UI + Rust backend" also splits the product into two technologies for no DSP benefit |
| **Qt via FFI** | Rejected | C++ toolchain weight, licensing complexity (LGPL dynamic-link obligations on Windows), enormous binaries, weakest fit for a "small Rust tool" |

**Renderer strategy:** default FemtoVG/WGPU backend, with automatic software-renderer fallback on Windows systems without OpenGL 2.0 (Slint handles this; verified in 1.18 changelog). Headless CI never touches the renderer — UI logic is tested through the model layer.

---

## 5. Audio I/O and codec design

### 5.1 Recording (cpal, target 192 kHz / 32-bit float)

- On "New Recording", enumerate the default input device's supported stream configs (cpal `supported_input_configs()`); select **192000 Hz / f32** if offered, else the highest supported rate ≥ 44.1 kHz.
- If the device cannot do 192 kHz, record at its best rate and **surface a non-blocking notice in the status bar** ("Recording at 48 kHz — device limit"). Honesty over silent downgrade.
- The input callback writes f32 frames into a disk-streaming writer (see §9.2) through an RT-safe SPSC queue — the audio callback never allocates, never locks, never touches the UI.
- Mono by default (vocal work), stereo passthrough supported on import.

### 5.2 Import

- **Probing:** symphonia `Hint`-free auto-detection by magic bytes; accept `.wav` (hound fast-path, covering PCM 8/16/24/32-bit + IEEE float 32/64, any rate, any channel count) and `.mp3` (symphonia MP3 decoder, full MPEG-1/2/2.5 Layer III).
- Decoded to planar/interleaved `f32` at the file's native sample rate (8 kHz–192 kHz supported by the engine); files at rates the engine handles natively are **never resampled on import** — resampling only happens on the preview path if the output device demands it.
- A peak mipmap (§8.4) is computed once at import (rayon-parallel) so waveform redraws are O(pixels), never O(samples).

### 5.3 Export

- **WAV:** hound writer, IEEE-float 32-bit at the session's native rate (lossless, professional default); optional 24-bit PCM int variant.
- **MP3:** mp3lame-encoder (LAME 3.100 bundled + statically compiled — verified no system liblame needed). Bitrate fully configurable: **CBR 32/64/96/128/160/192/256/320 kbps** and **VBR presets V0–V9**, plus mono/stereo. Export dialog previews expected file size.
- **Non-destructive by contract:** export always renders from the ORIGINAL source file through the current parameter set into a NEW file. The source file is never written to. Enforced by `mvl-io` API shape (`render_to(path, source, params)` — no in-place variant exists).

---

## 6. DSP engine design — the core innovation

### 6.0 Design philosophy

Every slider manipulates the voice at the level of **physically meaningful components** (F0, spectral envelope, breath/noise component) rather than applying a black-box effect. One shared STFT analysis drives all three processors, so they stay mutually consistent: the formant compensator knows what the pitch path did, and the breath detector knows where the voiced phrases are. All processing is phase-vocoder-family spectral processing on `realfft`, with all time-domain I/O handled by tested primitives (rubato, OLA windows). **No DSP is hand-rolled where a mature crate exists; the vocal-specific layers (envelope warping, breath analysis, phase locking) are original but built on rustfft/realfft primitives.**

### 6.1 Shared analysis layer (runs once, feeds everything)

- **F0 + voicing:** YIN (via `pitch-detection`, wrapped) on 1024-sample analysis frames (at 48 kHz; scaled at other rates), hop 256. Outputs per-frame: F0 in Hz (parabolic-interpolated), aperiodicity/voicing confidence, and RMS.
- **Spectral features per STFT frame:** spectral centroid, spectral flatness, high-band energy (4–10 kHz) / total energy ratio, autocorrelation peak strength.
- **Frame classification:** `Voiced` / `Sibilant` / `Breath` / `Silence`, hysteresis-smoothed over ±3 frames to prevent flicker at boundaries.
- This classification is what makes the Air/Breath module *musical rather than binary* (§6.4) and provides the voicing gates that protect consonants from the pitch/formant processors (unvoiced and sibilant frames receive conservative processing).

### 6.2 Pitch shifter — ±12 semitones, 1-cent resolution, timing preserved

**Algorithm: phase-locked phase-vocoder TSM → sinc resample → formant compensation** (the classic studio chain, each stage best-in-class):

1. **Time-scale modification (TSM)** with ratio `1/r` where `r = 2^(semitones/12)` (resolution 0.01 semitone internally, UI steps 1 cent = 0.01 st). Implementation: STFT (Hann, 75% overlap), phase propagation with **identity phase locking** (Laroche & Dolson peak-based locking): spectral peaks propagate their phases; bins in each peak's region inherit the peak's phase advance. This eliminates the "phase smearing / metallic" artifact of naive vocoders while keeping the analysis fully signal-agnostic and robust to breathy and noisy passages where epoch-based methods fail.
2. **Resampling** the stretched signal by `r` with rubato's windowed-sinc (32-tap, linear-phase) resampler — a 10-millisecond pitch change causes **zero** duration change (duration error measured and asserted in tests), and the sinc kernel keeps the spectrum clean of images and aliases.
3. **Formant compensation:** step 2 shifted formants by `r` along with pitch. The formant engine (§6.3) is fed a compensation factor of `1/r`, warping the spectral envelope back to its original position. Net result: **pitch moves, timbre (throat character) does not** — the anti-chipmunk guarantee, structurally.

**Why this beats the alternatives for this product:**

| Approach | Verdict | Reason |
|---|---|---|
| TD-PSOLA (e.g. `tdpsola` crate) | Rejected as dependency | Best-in-class for clean monophonic voice, but requires fragile epoch/pitch-mark detection; breaks on breathy onset, room reverb tails, bleed; the only Rust crate (v0.1.0, 3.2k downloads) is far below production maturity. Timing jitter at pitch marks conflicts with the "preserve natural timing" mandate |
| Naive phase vocoder | Rejected | Phasiness/metallic artifacts — exactly what we promise not to produce |
| Neural (DDSP/RVC/voice conversion) | Rejected for v1.0 | Best raw quality in research, but ships multi-MB models + ONNX/GPU inference stack, exploding binary/RAM/startup budgets and CI complexity; inference nondeterminism complicates the golden-file test strategy. Noted as roadmap item behind a feature flag |
| Simple resample ("chipmunk") | Rejected | Shifts formants with pitch; violates R8 |

**Edge handling:** unvoiced frames (breaths, consonants) use reduced phase locking to preserve noise character; DC and Nyquist bins bypass phase propagation; all gains normalized against overlapping Hann sum (constant-overlap-add verified: `sum(w²) ≡ 1` unit test).

### 6.3 Formant shifter — perceived vocal-tract length in millimetres

**Algorithm: true-envelope estimation → Bark-scale envelope warping → spectral ratio resynthesis.**

1. **Envelope estimation per frame:** iterative **true-envelope** (cepstral-domain, 4–6 iterations, F0-aware kernel width). Chosen over LPC because LPC misestimates formants when harmonics are sparse (high-pitched female/child voices) and pole-pairing is fragile; true-envelope converges to the physical envelope regardless of F0.
2. **Warping:** the envelope control points (64-point Bark-spaced representation) are frequency-mapped `f → f·g` with `g = L_ref / L` (uniform-tube physics: formant frequencies scale inversely with tract length; `L_ref = 170 mm`, adult-male average). Edges beyond Nyquist fold back with level reduction. User range: **L ∈ [100, 260] mm** ⇒ `g ∈ [0.65, 1.70]` — spans child (~110 mm) to very large male (~230 mm) perception. Slider is labeled in mm with a secondary musical-interval readout (e.g. "+4.7 st").
3. **Resynthesis:** `Y(f) = X(f) · Ẽ_warp(f) / Ẽ_orig(f)`, with the ratio regularized (envelope floor at −80 dBFS, ratio clamped ±18 dB, per-critical-band smoothing) so bins near spectral zeros don't explode and no "spectral holes"/birdie artifacts appear.
4. **Pitch independence is structural:** the formant engine operates purely on the spectral envelope; it has no knowledge of phase or F0 beyond the estimation kernel. Combined with §6.2's compensation contract, net formant displacement = user's mm setting, regardless of pitch-slider position. Asserted by test: with pitch = +12 st and formant = 170 mm (neutral), measured F1/F2 on output match input within ±3%.

### 6.4 Air & breath control — the signature feature

One bipolar slider **−100 % … +100 %** (with a 0.1 dB-precision gain readout and a dB-stepped mode), processing only frames the classifier tags `Breath` or `Sibilant`:

**Negative direction (remove breath & sibilance):**
- **Breath frames:** spectral-domain gain `−0 … −40 dB` (at −100 %), applied to the full band with **double smoothing** — over time (attack 5 ms / release 120 ms, so breaths duck musically instead of gating) and over frequency (critical-band smoothing, so the gain field never ripples between adjacent bins → no musical noise). Transient consonants (/p/, /t/, /k/) are detected via 3 ms energy-rise windows and exempted — articulation survives.
- **Sibilant frames (/s/, /ʃ/, /tʃ/):** split-band de-esser — dynamic attenuation of 4–10 kHz proportional to sibilance confidence, 0.1 dB steps, up to −24 dB, with the mid-band untouched so diction stays crisp.
- **Honesty guarantee:** removal never touches voiced frames; a "difference listen" button (renders input − output) lets users verify exactly what was removed.

**Positive direction (add warm airiness):**
- Gentle high-shelf (+0 … +6 dB above 6 kHz, first-order, phase-flat implementation in the spectral domain) — "warm" because the shelf is tilted: full lift only above 9 kHz, half-strength in the 5–9 kHz presence zone, nothing below.
- **Harmonic air synthesis:** the 2.5–6 kHz harmonic content of voiced frames is transposed +1 octave and re-injected at −24 … −12 dB, envelope-gated by the voicing detector (silence stays silent — no hiss floor added). This adds "air" that follows the voice's own spectrum instead of white noise.
- All added energy is bounded so true-peak never exceeds the source's true-peak +0.3 dB.

**Precision:** every gain parameter steps in 0.1 dB and the status readout shows applied gain to 0.1 dB; test asserts applied-vs-target gain within ±0.05 dB on synthetic breath segments.

### 6.5 Signal chain and parameter model

```
Source f32
  → [STFT analysis]  (shared: F0, voicing, features, envelope)
  → [1] Air/Breath   (spectral gain field, breath-frames only)
  → [2] Formant      (envelope warp: g_user / r_pitch compensation)
  → [3] Pitch        (PV-TSM × 1/r → rubato resample × r → back to original duration)
  → [4] True-peak safety limiter (only engages if |out| > source peak; −0.1 dBTP ceiling)
  → Output f32
```

Order rationale: breath cleaning first (so the pitcher never stretches noise the user removed); formant and pitch compensation share one envelope pass (single divide — one potential source of artifacts instead of two); the limiter is a transparency guard, not a compressor (engagement is asserted "never" in tests for moderate settings).

**Parameter struct** (all `Copy`, `PartialEq`, published to the audio thread via an atomic slot — no locks on the RT path):

```rust
pub struct VocalParams {
    pub pitch_semitones: f32,   // −12.00 .. +12.00, step 0.01 (1 cent)
    pub air_percent:    i32,    // −100 .. +100
    pub air_gain_db:    f32,    // readout / manual-dB mode, step 0.1
    pub tract_mm:       f32,    // 100.0 .. 260.0, 170.0 = neutral
}
```

### 6.6 Preview vs render — one engine, two profiles

| | Preview (real-time) | Render (export/apply) |
|---|---|---|
| STFT size | 512 @ 48 kHz (10.7 ms) | 2048 @ native rate |
| Hop | 128 (75% overlap) | 512 (75% overlap) |
| Phase locking | peak-based | peak-based + longer predictor |
| Algorithmic latency | **~16 ms** (+128-smp device buffer ≈ 2.7 ms → **< 20 ms total**) | n/a (offline, frame-parallel via rayon) |
| Source | 48 kHz preview copy (rubato-resampled from session, cached) | Full-rate original streamed from disk |

Both profiles call the same `VocalEngine::process_block()` with a `QualityProfile` — the no-fake-UI rule's strongest guarantee: **what you hear in preview is computed by the identical code the export uses.** Preview is muted (bit-exact bypass) only when all parameters are neutral, making A/B comparison instantaneous and honest.

### 6.7 Engine-level invariants (all unit-tested)

1. Bypass (neutral params) = **bit-exact** passthrough.
2. Pitch ±N st ⇒ duration identical (sample-count equality) and measured output F0 = input F0 × 2^(N/12) ± 0.5 %.
3. Formant neutral + any pitch ⇒ measured F1/F2 within ±3 % of source.
4. Formant shift ⇒ measured F0 unchanged ± 0.1 %.
5. Air −100 % ⇒ energy of breath-tagged segments reduced by the target dB ± 0.5 dB; voiced segments untouched ± 0.1 dB.
6. No NaN/Inf in any output sample, for all params, on all test signals (fuzz over parameter grid).
7. OLA windows sum to unity; resampler delay compensated to whole-sample alignment.

---

## 7. System architecture

### 7.1 Threading model

```
┌──────────── UI thread (Slint) ────────────┐
│ AppModel: params, transport state, zoom   │
│ publishes: Arc<AtomicSlot<VocalParams>>   │
│ subscribes: waveform/peaks, playhead,     │
│             export progress, engine stats │
└──────┬────────────────────────▲───────────┘
       │ rtrb SPSC (lock-free)  │ crossbeam-channel (event bus)
┌──────▼────────────────────────┴───────────┐
│ Audio thread (cpal callback)              │
│ pulls preview blocks from the RT chain,   │
│ feeds device; never allocates/locks       │
└──────▲────────────────────────────────────┘
┌──────┴────────────────────────────────────┐
│ Preview render thread                     │
│ VocalEngine(preview profile) + ring buffers│
└────────────────────────────────────────────┘
┌────────────────────────────────────────────┐
│ Worker pool (rayon) — offline jobs:        │
│ peak mipmap build, export render, analysis │
└────────────────────────────────────────────┘
```

- **Recording:** cpal input callback → rtrb → writer thread streams f32 to a temp WAV on disk (§9.2).
- **Parameter changes** take effect at the next preview hop boundary (< 3 ms); the engine crossfades parameter sets over one hop to prevent clicks.
- **Transport** (play/pause/stop/seek) is a message on the event bus; the preview thread owns the playhead clock and publishes position at 60 Hz for the status bar and playhead overlay.

### 7.2 Data model

- `Session { source: AudioSource, params: VocalParams, history: Vec<ParamsSnapshot> }`
- `AudioSource` is an abstraction over an in-memory buffer (short imports) or a disk-backed WAV reader (recordings and long files) — the engine consumes `&[f32]` blocks either way.
- Unlimited undo/redo of parameter snapshots (the source audio is immutable, so undo is trivially consistent).

---

## 8. UI/UX design system

### 8.1 Design language — "Precision Studio Dark"

Dark-neutral charcoal surfaces (never pure black), one disciplined accent (teal), color reserved strictly for meaning: **red = recording armed/capturing, green = transport playing, amber = warning, teal = focus/selection/primary action**. Dense but breathable layout tuned for hour-long sessions; 8-pt spacing grid; 6 px control radius / 10 px panel radius; 1 px `#2E323C` borders; subtle elevation via lighter surfaces, not heavy shadows. Motion is functional only (slider-thumb glow while dragging, 120 ms ease-out hover transitions, playhead at 60 fps).

### 8.2 Layout (LTR default, mirrors under RTL)

```
┌──────────────────────────────────────────────────────────────────────┐
│ ● Micro-Vocal Lab   [● New Recording] [⭱ Import File]   [⬇ Export]  │ ← toolbar
├───────────────────────────────────────────────────────┬──────────────┤
│  ⏮  ▶/⏸  ⏹        − ──────────────────── +  🔍 [fit] │ PITCH        │
│ ┌───────────────────────────────────────────────────┐ │ +3.00 st     │
│ │                                                   │ │ ▬▬▬●▬▬▬  [↺]│
│ │              WAVEFORM CANVAS                      │ │              │
│ │   (RMS body + peak outline, playhead)             │ │ AIR & BREATH │
│ │                                                   │ │ −12.0 dB     │
│ │                                                   │ │ ▬▬●▬▬▬▬  [↺]│
│ │                                                   │ │              │
│ └───────────────────────────────────────────────────┘ │ FORMANT      │
│  00:00:04.182 ── 00:00:11.940          48 kHz 32-bit │ 170 mm       │
├───────────────────────────────────────────────────────┴──────────────┤
│ ● ready · voice_01.wav · 48.0 kHz · 32-bit float · 01:24 ── EN │ عربي│ ← status bar
└──────────────────────────────────────────────────────────────────────┘
```

- **Top toolbar:** app identity dot, New Recording (red when armed), Import, Export (primary teal).
- **Transport bar:** play/pause/stop, zoom controls + fit + sample-level indicator, time readout (IBM Plex Mono, tabular).
- **Waveform canvas** fills the center; **right panel** hosts the three precision modules (mirrors to left in RTL).
- **Status bar:** state indicator, file info, sample rate / bit depth, position, language switcher.

### 8.3 The three precision modules (right panel)

Each module is a self-contained card with:
- **Title + icon**, live numeric value in IBM Plex Mono (e.g. `+3.00 st`, `−12.0 dB`, `170 mm`)
- **Custom slider** — full custom Slint component: track with center detent (0), filled portion toward current value, thumb with focus ring and drag glow; arrow keys step by 1 unit (1 cent / 0.1 dB / 1 mm), Shift+arrow = 10 units, Home = neutral; **double-click resets to neutral**; reset button `[↺]` per module; value is editable by click (numeric field)
- **Visual feedback:** pitch module shows note+cents (`F#2 +14¢`); air module shows a mini live meter of applied gain; formant shows mm + interval and a tiny vocal-tract glyph whose tube length tracks the value
- Sliders are live: every change publishes to the RT parameter slot at the next hop (< 3 ms), audible during playback with click-free crossfade.

### 8.4 Waveform canvas

- Peak **mipmap**: min/max/RMS pyramid computed once per file (rayon), zoom selects the level so rendering is O(pixels). Zoom range: full file → **sub-millisecond / sample-level** (at max zoom, individual samples shown as stem plot, R8).
- Rendered offscreen into a `SharedPixelBuffer<Rgba8Pixel>` in Rust, displayed as a Slint `Image`; playhead, selection and hover crosshair are lightweight overlay elements so 60 fps playback never re-renders the waveform image.
- Color: teal peak outline, 55 %-opacity teal RMS body, brighter core near playhead; grid lines at adaptive time subdivisions with mono labels.

### 8.5 Accessibility

- Full keyboard navigation with visible 2 px teal focus rings; every control reachable and operable by keyboard; sliders are focusable with documented key semantics.
- Tooltips on every interactive element (native Slint `accessible-description` + hover).
- High-contrast text tokens (WCAG AA: `#E7E9EE` on `#141519` ≈ 13.9:1; secondary `#9BA1AC` ≈ 7.2:1).
- Screen-reader support via Slint accessibility integration (accessibility tree exposure verified present in Slint 1.18).

### 8.6 Localization & RTL (English default, Arabic optional)

- **All strings through Fluent (`.ftl`) from day 1** — English catalog is the source of truth; Arabic catalog ships alongside. Language switcher in the status bar (`EN | عربي`); switch is instant (`update_all_translations()`), no restart.
- **RTL mechanics:** global `layout-direction` property flips every layout via `FlexboxLayoutDirection` (`row` ↔ `row-reverse`); all alignments are logical (`start`/`end`); text alignment resolves by direction; **IBM Plex Sans Arabic** is auto-selected when locale = `ar` (embedded, OFL). Digits stay Western Arabic numerals (standard in pro audio software, avoids metrology ambiguity).

### 8.7 Typography

| Role | Font | Rationale |
|---|---|---|
| UI text | **IBM Plex Sans** (400/500/600) | Engineered-for-UI grotesque, excellent at small sizes, neutral "instrument panel" character |
| Values & timecodes | **IBM Plex Mono** (400/500) | Tabular figures (no jitter while values change), one cohesive family, studio credibility |
| Arabic UI | **IBM Plex Sans Arabic** (400/500/600) | Same superfamily — consistent rhythm LTR↔RTL, full coverage, OFL |

Embedded in the binary at build time (slint-build custom fonts) → identical rendering on every OS, zero system-font dependency, no startup font-scan cost.

### 8.8 Color tokens

```
bg-base    #141519   panel      #1C1E24   raised     #23262E   border     #2E323C
text       #E7E9EE   text-2nd   #9BA1AC   text-muted #6B7280
accent     #2DD4BF   accent-dim #1FA396   focus-ring #2DD4BF (2px, 40% halo)
record     #F0524F   play       #3FDE8B   warn       #F5B93E
wave-peak  #2DD4BF   wave-rms   #2DD4BF @55%   selection  #2DD4BF @18%
```

---

## 9. Performance engineering (budgets and how we hit them)

| Budget | Target | Strategy |
|---|---|---|
| Binary size | < 50 MB | No webview; LTO (thin) + `strip` + `opt-level=3` + panic=abort in release; fonts subset-embedded; expect ~10–18 MB |
| RAM | < 200 MB typical | **Disk-backed session model** (§9.2): 192 kHz/32-f capture streams straight to temp WAV; RAM holds only the 48 kHz preview copy + peak mipmap + RT buffers. 5-min session ≈ 60–80 MB; hour-long sessions stay in budget because full-rate audio never lives in RAM |
| Preview latency | < 20 ms | 512-pt STFT @ 48 kHz (10.7 ms) + 128-sample device buffer (2.7 ms) + resampler group delay ≈ **16–19 ms**; measured and reported in the status bar (honest telemetry) |
| Cold start | < 1 s | No font scanning (embedded), no plugin scan, lazy device enumeration on first record, Slint UI compiled ahead of time |
| Render speed | ≥ 10× realtime | rayon frame-parallel offline render; 2048-pt STFT + 512 hop is SIMD-friendly |
| No leaks | — | `cargo test` with a soak test (process 100 blocks in a loop, assert RSS stable); static analysis via clippy; no `unsafe` in mvl-core/mvl-app (unsafe confined to nothing — we don't need any), no `unwrap()` on user data (clippy lint enforced in CI) |

### 9.2 Disk-backed session model (192 kHz done right)

Recording at 192 kHz/32-bit float consumes **768 kB/s per channel** — a 10-minute stereo take is ~0.9 GB and must never sit in RAM. The recorder streams f32 frames (via rtrb → writer thread) into a session temp WAV. On stop, three artifacts are built in background: (1) the full-rate source stays on disk as the render source, (2) a 48 kHz preview copy is resampled once (rubato) for the real-time path, (3) the peak mipmap is computed for the waveform. Exports stream from the full-rate file chunk-by-chunk. This is the same architecture class as DAWs and is what makes the RAM budget independent of recording length.

---

## 10. Testing & validation strategy

1. **Unit tests (mvl-core, headless, CI):** every module — YIN wrapper sanity (known sine → exact F0), OLA unity, true-envelope convergence on synthetic vowel spectra, envelope warp ratio on synthetic formant stacks, PV identity-phase-lock coherence, breath classifier on synthetic breath/sibilant/voiced fixtures, engine invariants §6.7.
2. **Golden/synthetic signals (committed in `testdata/`):** deterministic generated WAVs — 440 Hz harmonic stack with known formant positions, swept sine, shaped-noise "breath" segments with known PSD, formant-synthesized vowel sequence (gives objectively verifiable F1/F2), transient clicks. Plus a short CC0-licensed real vocal clip (sourcing verified in Phase 3) for naturalness checks.
3. **I/O round-trip tests:** WAV f32 write→read = bit-exact; MP3 encode→decode = bounded error (< −14 dB SNR at 320 kbps, spectral ceiling checks); export duration equality.
4. **RT-safety:** audio callback marked `#[no_alloc]`-style discipline — verified by code review checklist + a callback timing soak test on dev machines (callback never blocks on channels; SPSC only).
5. **Hardware smoke tests (dev machine, not CI):** record 5 s at 192 kHz → verify file; play through engine with params swept live; capture screenshots.
6. **Evidence pack (Phase 6):** every "works" claim backed by a screenshot; every "natural" claim backed by before/after WAV samples committed to `samples/`; performance numbers measured with `/usr/bin/time`, Task Manager/Activity Monitor, and reported honestly with methodology.

---

## 11. CI/CD plan (implemented in Phase 5)

- **Matrix:** `windows-latest` (MSVC x86_64), `macos-latest` (aarch64 + x86_64), `ubuntu-22.04` + `ubuntu-24.04` (X11/Wayland libs via apt: libxkbcommon, libwayland, etc.).
- **Pipeline:** `cargo fmt --check` → `cargo clippy --D warnings` → `cargo test --workspace` → `cargo build --release` → package per-OS archive (zip/tar.gz) → upload artifacts per commit + on tag.
- **Known constraint (documented honestly):** macOS artifacts unsigned/notarized — Gatekeeper requires `xattr -cr` or right-click-open; Windows SmartScreen may warn on first run. Audio-device tests cannot run headless in CI; they run as `--ignored` smoke tests on dev machines and the logic layers are fully covered headless.

---

## 12. Risk register

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| Phase-vocoder residual artifacts at extreme shifts (±12 st) | Medium | High | Identity phase locking + envelope preservation; quality validation on real vocals in Phase 3 with committed before/after samples; honest validation report |
| 192 kHz capture unsupported by common consumer devices | High | Medium | Capability negotiation with visible fallback notice (R4 handled honestly); engine is rate-agnostic |
| Preview latency misses 20 ms on some driver stacks | Medium | Medium | 512-pt profile engineered for it; measured latency displayed in-app; if a platform exceeds budget it's documented in the validation report, not hidden |
| LAME build friction on exotic toolchains | Low | Medium | Bundled static source, cc/autotools build proven by 1.1M downloads; CI matrix proves all three platforms |
| Slint GPL obligations | None | None | App is GPL-3.0 — compatible by design |
| RTL mirroring gaps in custom widgets | Medium | Low | Logical-direction rule from day 1; RTL screenshots in Phase 4 validation |
| cpal device quirks (WASAPI exclusive modes, Bluetooth) | Medium | Medium | Default-device strategy, graceful degradation, error surfacing in status bar |

---

## 13. License compliance matrix

| Component | License | Use |
|---|---|---|
| Micro-Vocal Lab | **GPL-3.0** | Full copyleft, free forever |
| Slint 1.18 | GPL-3.0 (royalty-free option) | GPL-3.0 tier used — compatible |
| cpal, hound, rustfft, realfft, rubato, rtrb, pitch-detection, crossbeam, rayon, thiserror | MIT / Apache-2.0 | Permissive — compatible |
| symphonia | MPL-2.0 | File-level copyleft — compatible in a GPL-3.0 work (notices preserved) |
| LAME 3.100 (via mp3lame-sys, statically linked) | LGPL-3.0 | Compatible with GPL-3.0 distribution; source availability honored by our public repo |
| IBM Plex fonts | OFL 1.1 | Embedded; license file shipped |

---

## 14. Phase roadmap (traceable to the execution protocol)

- **Phase 2** — workspace scaffold; cpal recorder with 192 kHz negotiation + disk streaming; hound WAV I/O; symphonia MP3 import; LAME MP3 export; transport player; I/O round-trip tests. *Exit: record→export→re-import bit-sound.*
- **Phase 3** — mvl-core: analysis layer, phase-locked PV pitch, true-envelope formant, air/breath engine; preview profile; invariants §6.7 tested; before/after samples committed.
- **Phase 4** — Slint UI: toolbar, waveform canvas with zoom, three precision modules, status bar, EN/AR + RTL, everything wired to the live engine (no fake UI).
- **Phase 5** — CI matrix, artifacts, full test suite green in CI, on-device verification with screenshots.
- **Phase 6** — full validation report with evidence pack, performance numbers, honest gaps, v1.0.0 tag.
