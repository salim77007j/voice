# Micro-Vocal Lab — Worklog

Append-only log. One entry per sub-item, per the phased execution protocol.

---

## 2026-10-04 — Phase 1: Research + Architecture Plan

**1.1 Repository** — Cloned https://github.com/salim77007j/voice (was empty); configured git identity; created base structure (`docs/`, `assets/fonts/`, `testdata/`, `crates/`, `.gitignore`, GPL-3.0 `LICENSE`).

**1.2 Rust audio ecosystem research** — Verified live versions from crates.io (2026-10-04): cpal 0.18.2 (2026-08-16), hound 3.5.1, symphonia 0.6.1 (2026-08-13), rustfft 6.4.1, realfft 3.5.0, rubato 5.0.1 (2026-10-01), rtrb 0.4.0, pitch-detection 0.3.0, mp3lame-encoder 0.2.5 (2026-08-20). Confirmed mp3lame-sys bundles LAME 3.100 source and builds it statically (verified in its Cargo.toml `include` + README) → no system MP3 dependency. Rejected `shine` MP3 encoder (unmaintained since 2018, 1.9k downloads). Research artifacts in `/home/z/my-project/research/` (outside repo).

**1.3 DSP approach research** — Compared TD-PSOLA (best naturalness on clean voice, fragile epoch detection, `tdpsola` crate too immature at v0.1.0/3.2k dl), phase vocoder with identity phase locking (robust, artifact-controlled, chosen), LPC (formant misestimation on high F0), neural DDSP/RVC (quality ceiling highest but violates size/RAM/startup budgets). Decision: phase-locked PV TSM → rubato sinc resample → true-envelope formant compensation chain; custom breath/sibilance analysis on shared STFT.

**1.4 UI framework research** — Slint 1.18.1 vs egui 0.36.2 vs iced 0.14.0 vs Tauri 2.12.1 vs Qt. Verified in Slint compiler source: `FlexboxLayoutDirection::row-reverse` + logical start/end alignment = genuine RTL mirroring; Fluent localization built in. egui/iced lack layout mirroring; Tauri violates RAM/binary budgets via webview. **Decision: Slint (GPL-3.0 tier, app is GPL-3.0).**

**1.5 Design decisions** — Fonts: IBM Plex Sans / Sans Arabic / Mono (one superfamily, OFL, tabular numerals, embedded). Palette: "Precision Studio Dark" (charcoal #141519/#1C1E24/#23262E, teal accent #2DD4BF, red record #F0524F, green play #3FDE8B). RAM strategy: disk-backed session model (192 kHz/32-f streams to temp WAV; RAM holds 48 kHz preview + peak mipmap only).

**1.6 Deliverables written** — `docs/ARCHITECTURE_PLAN.md` (14 sections: traceability, stack + evidence, UI framework analysis, DSP deep design per slider, system architecture, UI design system, performance budgets, testing strategy, CI plan, risk register, license matrix, roadmap), `README.md`, `LICENSE` (GPL-3.0 full text), this worklog.

**Phase 1 exit state:** plan committed and pushed; no code yet (by protocol). Waiting for "continue" to start Phase 2 (scaffold + audio I/O).

---

## 2026-10-04 — Phase 2: Project Scaffold + Audio I/O

**2.1 Workspace scaffold** (commit `d902cea`) — Cargo workspace: `mvl-core` (pure DSP), `mvl-io` (audio I/O), `mvl-app` (binary; UI lands Phase 4). `VocalParams` + `QualityProfile` in core; `InterleavedAudio` + typed `Error` in io. Environment: rustup 1.99.0 installed; ALSA dev headers extracted to user prefix `~/.local/alsa-dev` (no sudo in container; system `libasound.so.2` reused).

**2.2 WAV I/O** (commit `4d61f28`) — hound import (PCM 8/16/24/32-bit, float32; 8-bit unsigned bias handled) + export (float32 lossless default, 24-bit PCM option). RIFF magic sniff. 8 tests incl. bit-exact round-trips at 48k/192k and denormal extremes.

**2.3 MP3 + resampling** (commit `d0da59a`) — **symphonia pinned to 0.5.5**: the 2026-republished 0.6.x is a v2 dev preview whose own docs say "never use in production" (breaking API under same version number). LAME 3.100 (bundled static) via mp3lame-encoder 0.2.5: CBR 32–320 kbps, VBR V0–V9, IEEE-f32 input (no i16 quantization), Xing tag splice. rubato 5 FFT resampler (f64, delay-trimmed `process_all`). **Bug found by tests: LAME writes into Vec spare capacity — must reserve before every encode/flush call or SIGSEGV.** Unified `import()` with format auto-detection.

**2.4 Recorder** (commit `a8b7670`) — cpal 0.18 API absorbed (SampleRate=u32 alias, Display names, iterator configs, StreamConfig by value). Pure capability negotiation (F32@192k → fallback highest ≥44.1k → I16/U16; telephony-only rejected → device default fallback). RT callback: convert-to-f32 + rtrb push, zero alloc/lock; writer thread streams to disk WAV (RAM flat regardless of length). Overrun counting, NaN guard, graceful Drop.

**2.5 Player + transport** (commit `b8a6d82`) — pure `Transport` state machine (idempotent controls, clamped seek, natural end). Feeder thread + `SinkLogic` (testable consumer: pause=ring-preserving silence, seek=generation-counter flush, underrun-safe) + lock-free position (`base + fed − consumed`). One-shot device adaptation: rubato resample + mono↔stereo mix.

**Phase 2 exit state:** 3,235 LOC, 62 crates in tree, 48 tests green (2 hardware-ignored), fmt+clippy `-D warnings` clean, release binary 355 KB. Container has no real sound card — hardware tests are `#[ignore]`d for Phase 5 dev-machine verification. Gaps: 64-bit float WAV rejected loudly (hound limitation); player resamples up front (streamed preview path comes with Phase 3 engine). Waiting for "continue" to start Phase 3 (DSP engine).

---

## 2026-10-04 — Phase 3: DSP Engine

**3.1 STFT core** (commit `4f9e2be`) — streaming analyzer + exact-denominator OLA, left-flush pre-padding (perfect edge reconstruction), unity-COLA test, realfft/rustfft/pitch-detection/rubato added to mvl-core.

**3.2 Analysis layer** (commit `581b104`) — YIN via pitch-detection 0.3: **its clarity field is broken (−0.33 on pure tones)** → strictness 0.85 + MPM-style autocorrelation voicing; 2×-window retry for male F0 (85 Hz needs 2048 @48k); ≥96 kHz stride decimation; spectral features; Voiced/Sibilant/Breath/Silence classifier + causal majority smoothing; 1.3 ms transient detector; testsupport golden-signal generators.

**3.3 Pitch path** (commit `78548ac`) — identity-locked PV (Laroche-Dolson regions, reduced locking unvoiced, DC/Nyquist bypass — realfft requires exact-zero im there), Bresenham absolute-position hops (zero drift), rubato Async sinc 32-tap with discarded group delay (impulse-verified). +7 st on 220 Hz stack → 329.6 Hz (±0.5 %), duration sample-exact, periodicity clarity > 0.8.

**3.4 Formant warper** (commits `de6d712`, `ef31737`) — iterative cepstral true envelope (F0-aware lifter), Bark warp f→f·g, ±18 dB clamp, Nyquist fold-back. Magnitude-only: F0 preserved to 0.1 %.

**3.5 Air/breath engine** (commit `ee2ab5d`) — classifier-gated duck (−40 dB, 5/120 ms ballistics), confidence-weighted de-esser (−24 dB, 4–10 kHz), tilted shelf (+6 dB), octave-up harmonic air injection (voicing-gated). **Regression fixed:** left-flush pad frames misclassified voiced onsets as breath (−40 dB duck ate the first 150 ms) → partial-window frames fall back to Silence.

**3.6 Engine + limiter** (commit `5b05b98`) — full chain assembly, neutral bit-exact bypass, hop-granularity internal feeding (bulk == streamed, bit-identical), 4×-oversampled true-peak guard, `mvl-app process` CLI. **Three invariant-suite finds fixed:** bulk-push history trimming; frame-rate AM rumble from wobbling envelope estimates (harmonic-sampled envelopes + cross-frame one-pole + first-frame passthrough); spectral-empty-space gating by bin level. All §6.7 invariants green.

**3.7 Fixtures + docs** (this commit) — `testdata/` (7 deterministic sources) + `samples/` (8 before/after renders + README) committed with a bit-exact reproduction test; PHASE_3_REPORT.md.

**Phase 3 exit state:** 111 → 114 tests green (mvl-io 42+3, mvl-core 69; 4 ignored: 2 hardware + 2 regenerators), fmt+clippy clean, release 21.7× realtime render, binary 375 KB. Honest gaps: real-vocal listening tests deferred to Phase 5; pitch-param mid-stream changes rebuild the TSM path (transient); preview formant warp ~95 % of requested on dense harmonics (§6.6 trade-off, tested). Waiting for "continue" to start Phase 4 (Slint UI).

---

## 2026-10-04 — Phase 4: Slint UI, wired to the live engine

**4.1 UI foundation** — Slint 1.18.1 (winit + femtovg + software renderer; GPL-compatible licensing recorded). IBM Plex Sans/Arabic/Mono embedded: Sans statics unavailable from IBM channels → instanced from Google Fonts' variable TTF with fontTools (wght 400/500/600, name tables verified). Design tokens (`ui/theme.slint`), Path-drawn vector icons (no icon font), custom TextButton/IconButton with focus rings, custom **PrecisionSlider** (centre detent, arrow/Shift stepping, Home/double-click reset, RTL-aware mapping, editable numeric fields with lenient parse + canonical echo), ModuleCard ×3, WaveformView, app window per §8.2. **Mechanism correction:** Slint 1.18 i18n is gettext `.po` (not Fluent) — Arabic catalog at `translations/ar/LC_MESSAGES/mvl-app.po`, runtime switch via `select_bundled_translation`; RTL via `FlexboxLayout flex-direction: row-reverse` (no global layout-direction in 1.18).

**4.2 Rust glue** — `waveform.rs`: ×4 min/max/RMS mipmap + O(pixels) column renderer + sample-level stem plot + 1-2-5 ruler ladder + ViewSpan zoom math. `preview.rs`: live preview player — per-channel Preview-profile engines built **inside** the feeder thread (engine not `Send`), rtrb ring, generation-flushed seek, params slot at block granularity (<3 ms), lock-free position, applied-air-dB telemetry. `session.rs`: disk-backed recordings (§9.2 — 192 kHz stays on disk, 48 kHz preview streamed out; exports stream on-disk sources chunk-by-chunk), in-memory imports render via the exact Phase 3 offline path. `controller.rs`: all callbacks/properties wired; workers talk back over a crossbeam inbox drained by the 33 ms UI timer (no `Rc` or engine crosses threads). `headless.rs`: custom `slint::platform::Platform` over `MinimalSoftwareWindow` for offscreen tests + the `screenshot` subcommand.

**4.3 Debug war (all fixed + regression-tested)** — (1) `draw_column` cast a negative row to `usize` at canvas heights < 5 px → near-infinite loop (headless window starts 0×0) — clamped; (2) mipmap levels ≥2 derived min/max from the RMS column → peaks silently lost — now min-of-min/max-of-max; (3) `FlexboxLayout` main-axis growth needs explicit `alignment: stretch` (unlike HorizontalLayout) — the centre column collapsed until found by pixel forensics; (4) Slint's context is **thread-local** — headless platform installed per thread; (5) note+cents used the wrong octave convention — now MIDI (A4=69, octave increments at C).

**4.4 Verification + evidence** — 7 headless UI integration tests (state, session load, param sanitize/clamp/restore incl. §8.3's −12.0 dB = −30 %, zoom → sample level, RTL flip, honest no-device status, pixel-token screenshots) + 33 module tests. RTL mirroring verified **at the pixel level**: EN waveform x 0–948 / AR x 304–1248, toolbar reversed, status bar reversed, 55 % teal blend sampled exactly. Four real renders committed in `docs/phase4-screenshots/` (EN empty/loaded/alt-params, AR RTL). Release binary 27 MB (< 50 MB budget), self-check + screenshot verified in release mode.

**Phase 4 exit state:** 154 + 7 UI tests green (5 ignored: hardware/fixture by design), fmt + clippy `-D warnings` clean. PHASE_1_REPORT.md written retroactively (Phase 2 doc gap). Honest gaps in PHASE_4_REPORT §3: rfd portal dialogs untested in-container (no portal; CLI paths covered), very long high-rate *imports* still RAM-resident, 27 MB has a Phase 5 size pass queued, transport cluster intentionally LTR in RTL. Waiting for "continue" to start Phase 5 (CI + on-device verification).

---

## 2026-10-04 — Phase 5: CI matrix + on-device kit + size pass

**Session note** — the dev container was reset before this phase: rustup 1.99.0 reinstalled, the sudo-less ALSA prefix rebuilt (~/.local/alsa-dev, now from the Debian `libasound2-dev` .deb relocated over the system runtime lib; script + probe kept outside the repo), repo re-cloned from GitHub. The push credential stored in the old clone was lost — **Phase 5 commits are local until a token is supplied**; CI first runs on that push.

**5.1 CI + virtual audio** (commit `b2a66e5`) — `.github/workflows/ci.yml`: lint (fmt+clippy), 5-leg test matrix (ubuntu 22.04/24.04, windows, macos-14 arm64, macos-13 x86_64), release+artifact job gated on lint+test (linux built on 22.04 for the glibc 2.35 baseline; unsigned macOS documented). actionlint 1.7.7 clean. Null-PCM pacing settled with a C probe: **un-paced both directions** → the 3 real-device hw tests keep wall-clock assertions and stay `#[ignore]`d (§10.5); three new env-gated `virtual_*_smoke` tests (MVL_VIRTUAL_AUDIO=1) assert structure only and run on every Linux CI leg — cpal/ALSA enumeration→negotiation→stream→callback→disk now execute per push. Golden-sample test made **two-tier**: bit-exact on the fixture platform, ≤1e-4 (−80 dBFS) elsewhere (rustfft per-CPU butterfly drift would have failed the macOS legs on last bits).

**5.2 Size pass** (commit `20cb23e`) — cargo-bloat evidence **corrected the Phase 4 hypothesis**: `image` links at 172 KiB (default-features=false slint), the weight is structural (zbus 1.4 MiB shared by a11y+dialogs; resvg chain 1.5 MiB is a hard `i-slint-renderer-femtovg` → `i-slint-core { features=["image-decoders","svg"] }` edge). Real lever: **fat LTO + codegen-units=1 → 28.0 MB → 21.4 MB (−24 %)**. Verified after: 157 tests, self-check, EN/AR screenshots, 22.3× realtime. Not done on purpose: panic=abort (Phase 4 decision), font subsetting (coverage risk > 1.6 MB gain, documented).

**5.3 On-device kit** (commit `094bb16`) — `micro-vocal-lab selftest [--seconds N] [DIR]`: real-input capture (honest 192 kHz-fallback note), re-import, live-engine playback the operator listens to (pitch +3 st, air −30 %, tract 140 mm), offline WAV+MP3 export, EN/AR screenshot renders, PASS/FAIL/SKIP `report.txt` + exit code. Container smoke against the null device fails **honestly** (85 M dropped samples) and cascades skips — the failure path is itself verified. `docs/ONDEVICE.md`: the 10-minute human checklist (dialogs, live sliders, RTL, keyboard-only, listening verdict) feeding Phase 6's honest-gaps section.

**5.4 Docs** (this commit) — PHASE_5_REPORT.md, README (CI badge, Phase 5 status, selftest tour, Development section), this entry.

**Phase 5 exit state (in-container):** 160 tests passed / 0 failed / 5 ignored (2 hw + 2 fixture regen + 1 hw preview), fmt + clippy `-D warnings` clean, release 21.4 MB, actionlint clean, Linux CI leg proven locally with the virtual-audio env. Honest gaps: windows/macos CI legs unexecuted until push; on-device run pending a real machine; 21.4 MB above the §9 aspiration but 57 % under budget. Waiting for: push credential (or user push), then "continue" for Phase 6 (validation report + evidence pack + v1.0.0).
