# Phase 7 Report — v1.1.0 (P0 bug fixes · studio-rack UI · cross-platform re-verification)

**Commits:** `2631837` (7.1) · `04e4e1a` (7.2) · `b51259b` (7.3) · this report (7.4)
**Tag:** `v1.1.0` · **Workspace version:** `1.1.0` (was `0.1.0` — the version had never
been bumped through v1.0.0; fixed now, the binary reports `Micro-Vocal Lab 1.1.0`)

**Context.** After v1.0.0 shipped, a real user hit four critical bugs (could not
record, crashed on import, no output-device handling, general fragility) and
criticized the UI as "student work". Phase 7 was chartered to fix all four P0
bugs, redesign the UI against a user-supplied professional reference
(`reference.png`), re-verify on all five CI platforms, and cut v1.1.0.

**Final state:** 196 tests green on all 5 OS legs · clippy `-D warnings` clean ·
fmt clean · 4/4 release artifacts built and binary-verified · tag CI green.
Detail lives in the sub-reports; this document is the consolidated record.

---

## 1. Phase 7.1 — the four P0 bugs ([full report](PHASE_7_1_REPORT.md))

| Bug (user report) | Root cause | Fix highlights |
|---|---|---|
| **"No audio input device found"** on a normal laptop | `Recorder::start` opened *only* the default host's default input and gave up when that single pointer was unset; only 3 of 6 sample formats accepted | New `mvl-io::devices` module: every device on every host enumerated, never panics; recorder walks a unit-tested fallback order, fully opening each candidate; all 6 cpal formats (F32/I16/I32/U16/U32/U8); platform-specific recovery hints in errors; **device picker UI** (ComboBoxes over live inventory, pinning + automatic fallback) |
| **Import crashes the program** | three independent paths: `clamp(1, 0)` panic on zero-frame files (UI-thread death); workers without `catch_unwind` froze the busy flag on any library panic; no decode-size cap (OOM) | empty-mipmap guards + 2 pixel-level regression tests; all three worker paths panic-isolated into status-bar errors; 2 GiB decode cap with pre-decode size guard; zero-frame/non-file rejection; damaged-MP3 packets skip-and-count (partial audio survives); **1000-file deterministic fuzz — zero panics** |
| **No output device handling** | same default-only pattern in three players; preview was F32-only (an ALSA S16 default device could not play at all); channel adaptation was mono↔stereo only | output fallback chain + all 6 output formats everywhere; new shared `mvl-io::channels::adapt` (N→1, 1→N, N→M fold) replacing two private copies |
| **General robustness** | CI YAML suspected corrupt; no size caps; panic surface | CI file audited byte-level — valid (terminal artifact, not corruption); size caps + panic isolation close the fuzzed crash surface |

Tests: **160 → 189** (+29: 15 import-robustness incl. fuzz, 7 devices, 5 channels, 2 waveform regression).

## 2. Phase 7.2 — studio-rack UI redesign ([full report](PHASE_7_2_REPORT.md))

The v1.0.0 UI (flat teal cards) was rebuilt in the reference's **Audioprecise Pro**
language: deep-gunmetal rack panels (`#1A1D21`/`#242729`) with recessed display
wells and machined bevels, a two-hue discipline (**gold `#E6B800` = controls,
blue `#4A90E2` = audio data** — replacing teal-everywhere), skeuomorphic rotary
knobs, screw heads, engraved labels, and a green timecode readout.

Layout is a true rack: nameplate → recessed waveform well → three **channel
strips** (Pitch / Air & Breath / Formant) beside a **live analysis rack** →
transport with 44 px metal buttons → status bar.

Not just paint — new capability shipped:

- **`mvl-core::spectrum`**: pure-Rust 2048-pt realfft analyzer, 40 log bands
  (20 Hz–20 kHz, rate-stable), peak/RMS dBFS, no-panic fuzz-tested (+7 tests).
- **Live 40-band RTA** at 30 fps on the preview mix at the playhead, peak-hold
  caps, engraved frequency rules; **LED master meters** (24 segments, peak hold,
  latching clip); screenshots render *honest* analyzer state via `Controller::refresh()`.
- **Zero interaction regressions**: every callback, i18n string (7 new Arabic
  strings), RTL mirroring rule, and the propose-only parameter contract carried
  over byte-identical where possible.

Process: three VLM design-critique rounds, 4/10 → 7/10 → **8/10** ("iZotope/Antares
tier"). Evidence: `docs/phase7-evidence/ui-en-v1.1.0.png`, `ui-ar-v1.1.0.png`
(1440×900, real audio + real RTA data).

## 3. Phase 7.3 — cross-platform verification ([full report](PHASE_7_3_REPORT.md))

CI run on the 7.2 code commit (`04e4e1a`): **10/10 jobs green in 10m11s** —

- 5 test legs (ubuntu-22.04, ubuntu-24.04, windows-latest, macos-15 arm64 +
  Intel): **196/196 on every leg** (counts extracted from job logs)
- 4 release artifacts built and verified at the binary level after download:
  Linux ELF (executed — clean headless diagnostic, not a crash), Windows PE32+,
  macOS Mach-O arm64 + x86_64; 7.7–10.6 MB archives
- The docs-only commit (`b51259b`) re-ran CI: 10/10 green again
- Linux legs additionally exercise device paths against a virtual ALSA null PCM
  (`MVL_VIRTUAL_AUDIO=1`)
- Known constraint (unchanged since v1.0.0): headless CI runners cannot capture
  GUI screenshots; UI evidence is the Linux renders + Slint render-path
  equivalence across platforms

## 4. Phase 7.4 — release (this phase)

- **Version bump** `0.1.0 → 1.1.0` in the workspace manifest — the binary now
  reports `Micro-Vocal Lab 1.1.0` (`--help`, `self-check`); README refreshed
  (Phase 7 row, v1.1.0 release notes, new UI description, test count 196,
  artifact sizes, evidence links)
- **Stale diagnostic strings** corrected in `self-check` output ("phase 4 (UI)"
  → "studio rack UI"; "fluent-dark" → "studio-rack theme")
- **Final verification on the exact tagged tree** (fresh sandbox — the dev
  environment was rebuilt from scratch: Rust 1.99.0, local ALSA sysroot,
  re-clone from origin):
  - `cargo fmt --all --check` — clean
  - `cargo clippy --workspace --all-targets -- -D warnings` — clean
  - `cargo test --workspace` — **196 passed / 0 failed / 5 ignored**
    (ignored = hardware-gated on-device tests, by design)
  - `cargo build --release` — clean; `self-check` passes end-to-end on the
    release binary (engine render, ALSA host + input device enumeration,
    codecs, UI)
- **Tag `v1.1.0`** created on the final commit and pushed; the tag CI run
  (lint + 5×test + 4×release) is the release build users download

## 5. Quality gates — final scorecard

| Gate | Requirement | Result |
|---|---|---|
| Tests | all green, all platforms | **196/196 × 5 OS legs** (CI job logs) |
| Lint | clippy `-D warnings` | clean (local + CI) |
| Format | `cargo fmt --check` | clean (local + CI) |
| Crash safety | fuzz corpus | 1000 deterministic garbage files, **zero panics** |
| Import matrix | WAV 8/16/24/32-bit, f32, f64, mono→6ch, 8–192 kHz, damaged/truncated/empty | all handled, no crash, honest errors |
| Device fallback | no default-device dead end | full-chain open attempts on input *and* output, 6 formats, picker UI |
| UI quality | professional reference | VLM 8/10, RTL verified, zero interaction regressions |
| Artifacts | 4 platforms | built, downloaded, binary-format verified |
| Version | binary self-reports | `Micro-Vocal Lab 1.1.0` |

## 6. Known limitations (honest record)

1. **Real-hardware device-chain verification** still runs through the documented
   on-device kit (`docs/ONDEVICE.md`, `micro-vocal-lab selftest`) on machines
   with actual mic/speakers — CI can only verify the logic paths, not physical
   capture. The 5 hardware-gated `#[ignore]` tests remain the gate for that.
2. **macOS artifacts are unsigned** (since Phase 5): Gatekeeper needs
   `xattr -cr micro-vocal-lab` or right-click → Open on first run.
3. **Static screenshots can't show live behavior** (RTA animation, peak-hold
   decay, knob drag) — those exist only in the running app; the software
   renderer used for screenshots also stair-steps waveform edges that the
   desktop GL renderer smooths.
4. The workspace version string lagged reality at v1.0.0 (`0.1.0` internally);
   corrected in this release — if any external tooling pinned `0.1.0` it should
   re-pin to `1.1.0`.

## 7. Deliverables

- Tag **`v1.1.0`** on `main` — release binaries attached to the tag CI run as
  artifacts (linux-x86_64, windows-x86_64, macos-aarch64, macos-x86_64)
- Sub-reports: [`PHASE_7_1_REPORT.md`](PHASE_7_1_REPORT.md) ·
  [`PHASE_7_2_REPORT.md`](PHASE_7_2_REPORT.md) ·
  [`PHASE_7_3_REPORT.md`](PHASE_7_3_REPORT.md)
- UI evidence: [`docs/phase7-evidence/`](phase7-evidence/)

**Verdict: Phase 7 complete — v1.1.0 released.** All four user-reported P0 bugs
fixed with regression tests and a 1000-file fuzz gate; the UI rebuilt to the
professional reference with new real-time analysis capability; everything
re-verified on the full 5-OS matrix; version metadata corrected and tagged.
