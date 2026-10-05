# Phase 8.1 Report — Audit + V2 Strategy

**Deliverable:** [`docs/V2_STRATEGY.md`](V2_STRATEGY.md) (committed with this report)
**Baseline verified:** v1.1.1 tree (`9c854cc`) — `cargo test --workspace`:
**217 passed / 0 failed / 5 ignored** (matches the release claim exactly)
**Environment note:** dev container was reset again (fourth time); Rust 1.99.0
and the sudo-less ALSA prefix (`~/.local/alsa-dev`, relocated over the system
runtime lib) rebuilt per the Phase 5 recipe before testing.

---

## 1. Audit performed

### 1.1 Docs (all read)
README, ARCHITECTURE_PLAN (14 sections), ONDEVICE, WORKLOG (Phases 1–7
complete history), PHASE reports 1, 2, 3, 4, 5, 6, 7, 7.1, 7.2, 7.3, 7.5.

### 1.2 Code inventory
| Crate | LOC | Contents |
|---|---|---|
| mvl-core | 5,243 | STFT core, YIN/spectral analysis + classifier, identity-locked PV pitch path (693), true-envelope formant warper (647), breath engine (617), engine chain + limiter (907+139), spectrum analyzer (379), params (227) |
| mvl-io | 4,379 | recorder w/ 9-rung format ladder + consent diagnostics (1099), player (773), MP3 LAME (690), devices (388), WAV (366), channels/transport/resample |
| mvl-app | 4,676 Rust + 2,440 Slint | controller (1235), preview RT path (968), waveform mipmaps (634), selftest (533), session (522), headless/screenshot, studio-rack UI |
| **Total** | **~16.7 kLOC** | 222 `#[test]` fns → 217 run + 5 hardware-gated |

### 1.3 Test baseline
Fresh clone, fresh toolchain, full suite: **217/0/5**. fmt + clippy not yet
re-run on this box (they were green on the identical tree in the v1.1.1 tag CI;
they run again as the first gate of 8.2).

### 1.4 Competitive research (2026-10-05, web)
Melodyne 5 (€99–699, DNA, per-note editing), iZotope RX 11/12 ($399–1199,
Repair Assistant, Dialogue Isolate, combined De-noise/De-reverb, Spectral
Recovery), Auto-Tune Pro 11 / AutoTune 2026 (real-time Auto Mode, Graph Mode,
Harmony Player), Adobe Podcast Enhance v2 (free, cloud-only), Accentize
dxRevive Pro (neural restoration, beats RX Dialogue Isolate in reviews), Waves
Clarity VX / Vocal Rider / CLA Vocals, Krisp / NVIDIA Broadcast. DSP SOTA:
phase-locked PV refinements ("Phase Vocoder Done Right" class), ultra-light
DDSP vocoders, LPC+differentiable-DSP synthesis (MOS 4.36 reported),
DeepFilterNet-class real-time enhancement. Findings distilled into
V2_STRATEGY §2 (landscape) and §4 (technology choices).

## 2. The strategy (summary — full detail in V2_STRATEGY.md)

- **Positioning:** the only tool occupying the intersection of surgical
  editor / live chain / one-click enhancer — free, local, 20-something MB,
  5 OSes, EN+AR RTL. We out-scope Melodyne, out-weight RX, out-price
  everything, and are the local answer to Adobe Podcast.
- **Key decisions:** classical-first DSP for all V2 modules (zero bundled
  neural weights; a versioned ModelSlot is reserved for post-2.0 optional
  local packs); full pitch/formant decoupling via the existing cepstral
  true-envelope machinery; every module bit-exact on bypass; preview/render
  stay one algorithm; hard budgets (binary ≤ 30 MB, RSS ≤ 100/300 MB,
  preview < 10 ms, render ≥ 5× realtime) enforced by CI gates.
- **Rejected (documented):** bundled neural models, DNA-style polyphonic
  editing, plugin formats, cloud anything, FFI Vorbis ahead of pure-Rust FLAC.
- **Roadmap:** mission phases 8.2–8.12 adopted verbatim with measurable
  acceptance criteria per phase (V2_STRATEGY §5).
- **Benchmarking:** 50+ sample corpus (synthesized + CC0), objective metric
  table per module, golden-sample CI regression, MUSHRA-lite listening panels,
  competitor comparisons under explicit honesty rules (vendor claims labeled
  as claims; sandbox limits documented) → `docs/V2_BENCHMARKS.md` grows
  per phase.

## 3. Exit state

Strategy + report committed. **Push status: no GitHub token in this reset
environment** (the Phase 5 situation again) — commits are local until a
credential is supplied or the user pushes; CI runs on push as usual.

Waiting for "continue" to start **Phase 8.2 — playhead/audio sync bug fix**.
