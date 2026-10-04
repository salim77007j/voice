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
