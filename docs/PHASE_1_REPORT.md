# Phase 1 Report — Research & Architecture Plan

**Status: COMPLETE** (delivered at Phase 1; this report written retroactively during Phase 4 to close the documentation gap noted in the Phase 2 records) · Plan date: 2026-10-04

---

## 1. Scope

Phase 1 produced the decision record that Phases 2–6 build against, plus the repository foundation. No product code (per the execution protocol).

## 2. Deliverables (commit `8b1e15e`)

- **`docs/ARCHITECTURE_PLAN.md`** — 14 sections: vision & quality pillars, requirements traceability (R1–R8), stack choices, DSP design (shared STFT analysis; phase-locked phase-vocoder pitch shifting; true-envelope formant warping in tract millimetres; breath/sibilance spectral engine), preview-vs-render profiles, session/RAM model, the full UI/UX design system ("Precision Studio Dark"), performance budgets & strategy, testing strategy, CI plan, risk register, license compliance matrix and the phase roadmap.
- **`README.md`**, **`LICENSE`** (GPL-3.0), **`docs/WORKLOG.md`**, `.gitignore`.

## 3. Key decisions (with their §-references)

| Decision | Choice | Why |
|---|---|---|
| Language/toolkit | Pure Rust + Slint 1.18 | Native on all three OSes; GPU-accelerated; RTL expressible via row-reverse flexbox (egui/iced lack RTL mirroring); no webview (Tauri rejected on RAM/binary) |
| Audio I/O | cpal + hound + symphonia 0.5 + LAME | 192 kHz/32-f capture, WAV/MP3 both directions |
| DSP | rustfft/realfft + rubato + rtrb + pitch-detection | Phase-vocoder TSM → sinc resample → true-envelope formant compensation; PSOLA rejected (fragile), NN rejected (budget) |
| Recording model | disk-streamed sessions | 192 kHz stereo ≈ 768 kB/s/channel never sits in RAM (§9.2) |
| Localization | All strings through translation function from day 1; Arabic catalog; instant switch | R2 |
| Precision | 1 cent pitch / 0.1 dB air / 1 mm tract | R-brief |

Version claims were verified against crates.io on 2026-10-04; Slint RTL capability was verified from compiler source rather than docs.

## 4. Later corrections to Phase 1 assumptions (recorded for honesty)

- **Symphonia**: pinned to 0.5.5 in Phase 2 after the 2026-republished 0.6.x turned out to be a v2 development preview (its own docs say never use in production).
- **Translations**: the plan named Fluent `.ftl`; Slint 1.18 actually uses **gettext `.po`** bundles. Same capability, corrected during Phase 4 (see PHASE_4_REPORT §1.6).
- **Preview transport**: the plan's §6.6 "< 20 ms total latency" was confirmed by the Phase 3 measurements (algorithmic ≈ 10.7 ms @ 48 kHz).

## 5. Verification

- All dependency versions fetched and confirmed from crates.io.
- Repo pushed (`8b1e15e`) with plan, README, license and worklog.
