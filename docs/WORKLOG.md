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
