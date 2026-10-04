# Phase 5 Report — CI matrix, on-device verification kit, binary-size pass

**Status: COMPLETE (in-container) · Date: 2026-10-04 · Tests: 160 passed / 0 failed / 5 ignored (2 hardware + 2 fixture regenerators + 1 hardware preview) · fmt + clippy `-D warnings` clean · release binary 21.4 MB (−24 % vs Phase 4, budget < 50 MB)**

Phase 5 scope (ARCHITECTURE_PLAN §14): *"CI matrix, artifacts, full test suite green in CI, on-device verification with screenshots."* — CI, artifacts and the size pass are delivered and verified as far as this container can verify them (the Linux CI leg is proven bit-for-bit locally); the on-device part ships as a **verification kit** (`selftest` subcommand + `docs/ONDEVICE.md`), because this machine has no real microphone, speakers or display — faking that evidence is exactly what this project refuses to do.

**Session note (honest):** the development container was reset between Phase 4 and Phase 5 — the Rust toolchain, the sudo-less ALSA dev prefix (`~/.local/alsa-dev`) and the git push credentials stored in the old clone were all gone. The toolchain and ALSA prefix were rebuilt (same approach as Phase 2, now scripted in the outer worklog); the Phase 1–4 history was re-cloned from GitHub intact. **The push credential was not recoverable, so the four Phase 5 commits are local until a token is supplied — the CI jobs first run on that push.**

---

## 1. What was built

### 1.1 CI matrix + per-OS artifacts (commit `b2a66e5`)

`.github/workflows/ci.yml` — three job families, validated with actionlint 1.7.7 (clean):

- **`lint`** (ubuntu-24.04): `cargo fmt --all --check` → `cargo clippy --workspace --all-targets -- -D warnings`.
- **`test`** — 5 legs: `ubuntu-22.04`, `ubuntu-24.04`, `windows-latest`, `macos-14` (arm64), `macos-13` (x86_64), `fail-fast: false`, rust-cache, ALSA headers via apt on Linux.
- **`release`** — gated on lint+test: `cargo build --release` + packaged artifact (`tar.gz`/`zip` with binary + LICENSE + README) for `linux-x86_64` (built on 22.04 → glibc 2.35 baseline), `windows-x86_64`, `macos-aarch64`, `macos-x86_64`; uploaded per commit on `main` and on `v*` tags, 30-day retention. macOS stays unsigned (documented §11 constraint).

**Virtual-audio smoke in CI.** A C probe (`scripts/probe-null.c`, compiled against the ALSA headers) settled empirically that the `null` PCM is **un-paced** — capture/playback transfer instantly, no hardware clock. The three real-device hardware tests therefore keep their wall-clock assertions and stay `#[ignore]`d (plan §10.5: dev machines). Instead three new **`virtual_*_smoke` tests** run in the normal suite whenever `MVL_VIRTUAL_AUDIO=1` (the Linux CI legs + this container) and assert *structure without timing*: device enumeration, capability negotiation, stream build, callback delivery, f32 conversion, ring transport, disk streaming, WAV finalization; play/pause/stop state machine; the full preview path (engine construction inside the feeder thread). All three pass against the null device — the cpal/ALSA path now executes on **every push**, not just on dev machines.

**Cross-platform golden-sample tolerance.** The Phase 3/4 `engine_reproduces_committed_samples_bit_exact` test compared raw bit patterns — sound on the generating platform, but rustfft selects butterfly implementations per CPU (SSE/AVX/FMA vs NEON), so the macOS legs would almost certainly have failed on last-bit drift, taking "suite green in CI" with them. The test is now two-tier: **bit-exact** wherever the FFT dispatch matches the fixture-generating platform, otherwise every sample must sit within **1e-4 (−80 dBFS)** — a real algorithm change moves samples by orders of magnitude more, so the regression net still fails loudly. The neutral-bypass test stays bit-exact everywhere (pure copy, no FFT).

### 1.2 Binary-size pass (commit `20cb23e`)

Evidence first (`cargo-bloat`, per-crate .text): the Phase 4 hypothesis — *"27 MB mainly due to Slint's default image-decoder stack (resvg/webp/jpeg/gif)"* — was **wrong**. The resolved feature graph (slint built with `default-features = false`) links `image` at only **172 KiB**. The actual weight: `std` 1.9 MiB, `zbus` 1.4 MiB (shared by accessibility/AT-SPI and the xdg-portal file dialogs — both product features), `mvl_core` 1.1 MiB, the Slint stack ~3.1 MiB, the resvg/SVG chain ~1.5 MiB — which is a **hard, non-optional dependency edge of `i-slint-renderer-femtovg`** (`i-slint-core = { features = ["image-decoders", "svg"] }`), i.e. the price of the GPU renderer, not a trimmable option.

What actually worked: **fat LTO + `codegen-units = 1`** — cross-crate dead-code elimination:

| | Phase 4 | Phase 5 | |
|---|---|---|---|
| Release profile | thin LTO, CGU 16, strip | **fat LTO, CGU 1, strip** | plan §9 |
| Binary size | 28.0 MB | **21.4 MB** | **−24 %**, 57 % headroom under the 50 MB budget |

Verified after the change: full suite (160/160), `self-check`, EN + AR screenshot renders, offline render at 22.3× realtime. Deliberately **not** done: `panic = "abort"` (Phase 4 decision — audio backends unwind callbacks; revisit Phase 6 only if needed) and font subsetting (~1.6 MB of embedded IBM Plex; §9 mentioned it, but full statics guarantee glyph coverage for any future string — documented trade-off, not silently skipped).

### 1.3 On-device verification kit (commit `094bb16`)

`micro-vocal-lab selftest [--seconds N] [DIR]` — one command a human runs on a real desktop; it records from the **default input device** (with the honest 192 kHz-fallback note when the device caps lower), re-imports the take, plays it through the **live engine** (audible: pitch +3 st, air −30 %, tract 140 mm — the operator *listens*), exports the offline Render-profile WAV + MP3, renders the real UI in EN and AR/RTL, and writes a PASS/FAIL/SKIP `report.txt` + exit code. Machine-checkable claims are checked by machine (no drops, ±15 % duration, wall-clock-pacing of engine playback, ≥1× realtime render); perceptual claims are deliberately left to the human. `docs/ONDEVICE.md` is the 10-minute manual protocol that consumes it (dialogs, live sliders, RTL switch, keyboard-only pass, listening verdict) and feeds Phase 6's honest-gaps section.

Container smoke of the kit: run against the un-paced null device it **fails honestly** at the dropped-samples check (85 M samples — exactly what a null PCM produces), cascades SKIPs, writes the report, exits 1. On a real (clock-paced) device the same code path passes; the components of the happy path (import, engine render, exports, screenshot renders) are each already under test.

### 1.4 Docs (this commit)

README: CI badge, Phase 5 status row, `selftest` in the command tour, Development section (prereqs, the exact CI commands, artifact notes). This report; WORKLOG entry.

---

## 2. Verification summary

| Check | Result |
|---|---|
| `cargo test --workspace` (no env) | **154 passed**, 5 ignored — Phase 4 baseline reproduced after the container reset |
| `cargo test --workspace` with `MVL_VIRTUAL_AUDIO=1` + null PCM (= Linux CI leg) | **157 passed** (154 + 3 virtual smokes), 0 failed |
| `cargo test --workspace` after 5.2 + 5.3 | **160 passed** (157 + 3 selftest units), 5 ignored, 0 failed |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean |
| `cargo fmt --all --check` | clean |
| `actionlint 1.7.7` on `ci.yml` | clean |
| Release build + size | 21.41 MB; self-check, EN/AR screenshots, 22.3× realtime render verified |
| Null-PCM pacing probe | compiled + run; both directions un-paced (evidence for the test design) |

---

## 3. Honest gaps

1. **CI has not executed on GitHub yet** — the four Phase 5 commits are local: the container reset lost the push credential. The Linux leg is proven locally (identical commands), `ci.yml` passes actionlint, but the windows/macos legs are **unexecuted** until the user pushes (or supplies a token). Risk register: LAME-on-MSVC builds fine per the mp3lame-encoder crate's own CI, but that is their claim, not our observation.
2. **On-device run pending** — by definition it needs a real machine; the kit exists, `docs/ONDEVICE.md` documents it, no evidence is claimed until someone runs it.
3. **21.4 MB is above the §9 aspiration** (10–18 MB) though well inside the 50 MB budget; the remaining weight is structural (SVG chain behind femtovg, zbus behind a11y + dialogs, fonts, DSP) — documented in §1.2 rather than cut.
4. **Cross-platform tolerance tier** (1e-4) is a policy call — bit-exactness is still enforced on the generating platform; if a Phase 6 release wants bit-identical cross-platform output it needs fixed-point or platform-pinned FFT dispatch (out of scope, noted).
5. **Very long high-rate imports still RAM-resident** (Phase 4 §3.7) — unchanged; still deferred unless real usage demands it.
6. **`system-tray`** stays off, `panic=unwind` stays on — both documented decisions.

---

## 4. Next (Phase 6 per §14)

Push Phase 5 (needs credential) → CI green on all 5 legs → run `selftest` + the ONDEVICE checklist on a real desktop → full validation report with the evidence pack (selftest outputs, on-device screenshots, listening notes), measured performance numbers (latency, realtime factor, cold start, RAM), honest gaps, `v1.0.0` tag.
