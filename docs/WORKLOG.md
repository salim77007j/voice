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

---

## 2026-10-04 — Phase 6: full validation + evidence pack + v1.0.0

**Session start** — user supplied the push token with "continue"; Phase 5's four local commits (`b2a66e5`…`dbf0251`) pushed (`bc5e25c..dbf0251`), triggering the repository's first-ever CI run.

**6.0 CI first-run incident + hotfix** (commit `879ca7d`) — run #1 (`dbf0251`) failed both ubuntu-24.04 legs: `yeslogic-fontconfig-sys` 6.0.1 (via `fontique`, Slint 1.18's font enumeration) panicked in build.rs — the 24.04 runner image ships no fontconfig dev files, while the dev container and the 22.04 image do (which is why every local "CI-leg proven" run passed; honest lesson recorded). Dependency audit via `cargo tree -i` on x86_64-unknown-linux-gnu: hard-linked system libs are exactly ALSA + fontconfig (wayland/glutin_glx are dlopen; libudev/input/libseat-sys not in the tree). All three Linux apt steps now install `libasound2-dev libfontconfig1-dev`; actionlint 1.7.7 clean. Run #2 (`879ca7d`): **all 5 test legs + lint green**, release artifacts built.

**6.1 Local full validation** — reproduced on the exact hotfix commit: `fmt --check` PASS, `clippy -D warnings` PASS, `MVL_VIRTUAL_AUDIO=1 cargo test --workspace` **160/0/5 in 65 s**, release build 21.64 MiB (P5 recorded 21.41 — fat-LTO link nondeterminism ~1 %, both numbers reported), `self-check` + EN/AR headless screenshots + two-profile offline renders all PASS.

**6.2 Performance evidence** (`docs/phase6-evidence/perf.txt`) — cold start **36 ms** (median of 5, spawn→boot→smoke-render→exit), peak RSS **8.4 MiB** boot / **22.1/22.4 MiB** full UI EN/AR / **10.2 MiB** render; offline render **22.7× realtime** (Render profile, three modules active) and **16.0×** (Preview profile); preview algorithmic latency 10.7 ms @ 48 kHz (test-asserted, unchanged). Every §9 budget met with 9–28× margin. Methodology (no `/usr/bin/time` in container): monotonic wall clock + `/proc/<pid>/status` VmHWM polling, script kept outside the repo.

**6.3 Evidence pack + reports** (this commit) — `docs/phase6-evidence/`: `perf.txt`, `ci-run.txt` (machine-captured API snapshot of run #2: per-job results, durations, artifact sizes), fresh EN + AR/RTL UI PNGs, two-profile render WAVs. `docs/PHASE_6_REPORT.md`: verdict table, validation matrix, budget-vs-measured table, test-quality narrative (48→160 across phases, invariant spine), the CI incident record, final honest gaps (on-device run still pending a human; 21.6 MB vs 10–18 aspiration; 1e-4 cross-platform tolerance policy; long high-rate imports RAM-resident; unsigned macOS; standing decisions). README: Phase 6 ✅ + v1.0.0 numbers.

**6.3.5 CI matrix migration** (commit `47e6e21`) — runs #2 (main) and #3 (tag, same tree) went 5/6 green in minutes, then `test (macos-13-x86_64)` sat unassigned in the queue ~50 min in BOTH runs: **macos-13 is delisted from the hosted fleet** (actions/runner-images README, Sept 2026: only `macos-15` arm64 + `macos-15-intel` x64 remain GA; macos-14 deprecated). Runs cancelled (5-green job states captured into perf.txt first), matrix migrated `macos-13 → macos-15-intel`, `macos-14 → macos-15` (label verified: ~30k workflow files use it), actionlint upgraded 1.7.7 → 1.7.12 (old build's label list predates the label).

**6.4 Release** — annotated tag `v1.0.0` first pushed on `879ca7d`; when the macos-13 delisting made its release gate unreachable, the tag was **re-pointed to `47e6e21`** (fixed workflow + this documentation; the tag was under an hour old with no consumers — the re-point and its reason are recorded here rather than silently buried). The tag push ran the full pipeline on the tagged tree: **run #5 completed success, 10/10 jobs** (lint 39 s; tests ubuntu 22.04/24.04/windows/macos-15-arm64/macos-15-intel; releases linux/windows/macos-aarch64/macos-x86_64) and attached the four platform artifacts (7.4-10.2 MiB archives, machine-captured in phase6-evidence/ci-run.txt). Run #4 (main, identical tree) also finished success in full.

**Phase 6 exit state:** protocol complete. v1.0.0 = 47e6e21 with green end-to-end CI on GA runner labels; 160 tests / fmt / clippy green locally and on all 5 legs; every §9 budget met with 9-28x margin; two live incidents (fontconfig gap, macos-13 delisting) caught, fixed and documented; final honest gaps enumerated in PHASE_6_REPORT §6 (on-device run pending a human).


---

## 2026-10-05 — Phase 8.1: V2 audit + strategy (V2 campaign opens)

**8.1.1 Environment rebuild** — container reset again (4th time): rustup 1.99.0 reinstalled, sudo-less ALSA prefix rebuilt at `~/.local/alsa-dev` (libasound2-dev .deb extracted, `.so` relocated over the system runtime; `PKG_CONFIG_PATH` + `LIBRARY_PATH` both needed — the latter was newly required for the linker this time, recorded for the next reset). Repo re-cloned; origin/main verified at `v1.1.1` (`9c854cc`).

**8.1.2 Audit** — all 13 docs + WORKLOG read; code inventory: mvl-core 5,243 / mvl-io 4,379 / mvl-app 4,676 Rust + 2,440 Slint LOC, 222 test fns. Full suite on the fresh toolchain: **217 passed / 0 failed / 5 ignored** — baseline claim reproduced exactly.

**8.1.3 Competitive + DSP research (live web)** — Melodyne 5 (€99–699, DNA), RX 11/12 ($399–1199, Dialogue Isolate, combined De-noise/De-reverb, Spectral Recovery), Auto-Tune Pro 11 / AutoTune 2026, Adobe Podcast Enhance v2 (cloud-only — our privacy foil), Accentize dxRevive Pro (neural restoration, beats RX in reviews), Waves Clarity VX/Vocal Rider/CLA, Krisp/NVIDIA Broadcast; DSP: phase-locked PV refinements, ultra-light DDSP vocoders, LPC+differentiable DSP (MOS 4.36), DeepFilterNet-class enhancement.

**8.1.4 Strategy** — `docs/V2_STRATEGY.md` written: honest audit (§1), landscape + positioning (§2), add/improve/reject tables (§3), classical-first DSP selection per module with a reserved ModelSlot (§4), hard perf/size budgets, roadmap 8.2–8.12 with measurable acceptance criteria (§5), benchmarking methodology + honesty rules (§6), risk register (§7), v2.0.0 quality bar (§8). Rejections documented with rationale: bundled neural weights, DNA polyphony, plugin formats, cloud anything, FFI Vorbis before pure-Rust FLAC.

**Phase 8.1 exit state:** strategy + report + this entry committed locally. No push credential in this environment (Phase 5 precedent: commits stay local until a token is supplied). Baseline 217/217 verified. Waiting for "continue" → Phase 8.2 (playhead sync bug).

---

## 2026-10-05 — Phase 8.2: playhead/audio sync fix (sync campaign)

**Session start** — container reset (5th time): rustup 1.99.0 + rustfmt/clippy components installed; sudo-less ALSA prefix rebuilt at `~/.local/alsa-dev` from the trixie `libasound2-dev` .deb (headers only — the package's `libasound.so` symlink is dangling on Debian 13, re-pointed at the system `libasound.so.2.0.0`; pkg-config + `RUSTFLAGS="-L native=…"` both required, lld ignores `LIBRARY_PATH` — recorded for the next reset). Repo re-cloned; origin/main verified already at `9c854cc`…`1a788e9` (Phase 8.1 had landed on the remote despite the earlier no-token note), so nothing was pending to push.

**8.2.1 Root cause** — `position_seconds()` in both players computed `base + fed − consumed` = the **feeder write cursor** (ring occupancy ≈ 0.4–0.5 s in steady state), so the playhead led the audible audio by the whole ring fill and jittered with every ring top-up; the device's own buffer latency (10–50 ms) was additionally not subtracted; rate adaptation was already structurally correct but untested for the seconds-mapping; clock drift was masked by the dominant bug. Full writeup in `docs/PHASE_8_2_REPORT.md` §1.

**8.2.2 Fix** — new `crates/mvl-io/src/playhead.rs`: seqlock-protected playhead anchor written by the audio callback (frame position of the first popped sample + `Instant::now()` + cpal `playback − callback` device latency; 4 atomic stores + 1 clock read — RT-safe, no alloc/lock), and `audible_position_seconds()` extrapolating on the UI thread: `audible(T) = first_frame + (T − callback − latency)·rate`, clamped to the handed cursor (underruns freeze instead of running ahead of silence) and the track length. Both `mvl_io::player` and `mvl_app::preview` sinks publish anchors in all six sample-format paths; flushed (discarded) ring content is no longer counted as handed; bump ordering publishes base/counters before the generation increment; pause freezes at the resume point. `Controller::park_playhead()` added so screenshot renders publish line + timecode + visibility together.

**8.2.3 Verification** — 13 new tests (9 anchor unit + flushed≠handed + anchor-position + **e2e sync acceptance: real feeder + ring + sink under a simulated paced device, |reported − audible| ≤ 5 ms at every probe** + device-rate-mapping after 44.1→48 adaptation). Full suite **230 passed / 0 failed / 5 ignored** (217 baseline + 13, zero regressions); fmt + clippy `-D warnings` clean; `MVL_VIRTUAL_AUDIO=1` virtual smokes green (real cpal/ALSA path through the anchor code). Evidence renders: `docs/phase8-evidence/playhead-at-{0.30,0.75,1.20-ar}s.png` (line and timecode agree; RTL render intact). ONDEVICE row 5 sharpened with the sync protocol (44.1 + 48 kHz files, ≤ 1 UI frame, pause/seek/RTL).

**Phase 8.2 exit state:** fix + tests + docs committed and pushed (token supplied this session). Waiting for "continue" → Phase 8.3 (EQ panel, 4-band parametric).

---

## 2026-10-05 — Phase 8.3: 4-band parametric EQ panel (+ CI cache keys, macOS e2e hotfix)

**Request 1 — CI caching** — `Swatinem/rust-cache@v2` was already in all three Rust-compiling job families since Phase 5.1; added the per-job `key` partition (`lint` / `test` / `release`) so the release profile stops thrashing the debug target dirs (`4c8e4da`). First run per key = expected MISS; next run restored `full match: true`.

**8.2 follow-up — macOS e2e flake** — CI caught `e2e_playhead_matches_audible_audio_within_5ms` on both macOS legs (reported 24.45 ms ahead). Root cause: the *test's* simulated device reported a constant 21 ms latency and asserted against a wall-clock formula; a loaded runner bunches callbacks (6 probes in 25.6 ms observed) and a real device's reported latency grows with its queue during bursts. The sim now models the pipeline honestly (queue seeded with 21 ms pre-roll, drains in real time, per-callback pre-block queue depth as the reported latency) — player anchor vs sim truth is algebraically exact between events incl. bursts/drains/underruns; deterministic schedule stresses all three regimes; `playhead_uses_device_rate_after_resample` de-flaked the same way. 10× local stress green; CI run `7fc2a1d` fully green (10/10 jobs).

**8.3.1 DSP** — new `mvl-core/src/eq.rs`: RBJ cookbook biquads (shared `α = sin ω₀/2Q` convention incl. shelves — NaN-free across the full range; `f₀` clamped < 0.45·Fs), TDF2 in f64, 4 fixed-position bands (low shelf / bells / high shelf), `EqParams` (master + per-band bypass) with `sanitized()`, inert-band skip = bit-exact (no −0.0 flips), lazy per-triple coefficient redesign, `EqProcessor`, analytic `response_db` + 129-point log grid for the UI, five presets with documented musical intent.

**8.3.2 Integration** — `VocalParams` grew `eq: EqParams` (still plain `Copy`, same parameter channel). Engine applies the EQ after the true-peak guard in `process` + `flush`; whole-neutral → bit-exact bypass (invariant #1 intact); vocal-neutral + EQ-only → biquads run with the STFT machinery *off* (instant A/B preserved). Preview, both export paths, CLI and selftest inherit the EQ with zero plumbing (what you hear is what you export).

**8.3.3 UI** — new `widgets/eq-panel.slint`: full-width rack row — response curve (controller-computed RBJ math published as line + fill path commands; `ImageFit.fill` needed — default `contain` letterboxes the fixed-ratio viewbox, caught during evidence rendering), four band strips (bypass LED, log-sweep frequency/Q knobs + linear gain knob via the Knob's new `logarithmic`/`enabled` modes, mono readouts), master ON/BYPASS, preset ComboBox (labels via `@tr` in Slint; highlight = exact equality), Reset. Controller publishes canonical values + curve + texts and pushes params to the preview player; nothing is fabricated in the UI. Full `ar` catalog section (band titles, presets, labels); panel mirrors under RTL, curve axis stays LTR (same convention as RTA/timeline).

**8.3.4 Verification** — 17 new tests (acceptance: measured response = analytic biquad at 5 freqs × 3 band kinds ≤ 0.05 dB; bypass bit-exactness with a −0.0 sentinel; stability Jury sweep over the full parameter range; EQ-only path = bare biquads; EQ after chain; rate redesign). Suite: **247 passed / 0 failed / 3 ignored** (230 + 17); fmt + clippy `-D warnings` clean; virtual-audio smokes green; CI 10/10 jobs green with cache hits. Evidence: `docs/phase8-evidence/eq-panel-{en,ar}.png` (Vocal Presence EN / De-Mud AR-RTL).

**Phase 8.3 exit state:** pushed. Waiting for "continue" → Phase 8.4 (compressor). Honest gaps (block-rate zipper, preset-match placeholder, shelf-Q semantics) enumerated in PHASE_8_3_REPORT §5.
