# Phase 2 Report — Project Scaffold + Audio I/O

**Status: COMPLETE** · Date: 2026-10-04 · Commits: `d902cea` → `b8a6d82` (5 commits, each building green)

---

## 1. What was built

A three-crate Cargo workspace implementing the complete audio I/O layer
per `docs/ARCHITECTURE_PLAN.md` §5, with every user-facing failure path
returning typed errors (no `unwrap()` on user data, no panics).

| Crate | Role | Contents |
|---|---|---|
| `mvl-core` | Pure DSP foundation | `VocalParams` (the three precision sliders: clamped, NaN-safe, ratio math), `QualityProfile` (preview/render STFT shapes) |
| `mvl-io` | Audio I/O | WAV import/export (hound), MP3 import (symphonia) + export (LAME 3.100, bundled/static), FFT resampling (rubato), 192 kHz recorder (cpal + rtrb disk streaming), player with transport (cpal + rtrb + crossbeam) |
| `mvl-app` | Binary | Boot self-check (engine params, audio host, input device, codec versions). Slint UI lands in Phase 4 |

## 2. Sub-item completion (protocol checklist)

| # | Protocol item | State | Evidence |
|---|---|---|---|
| 1 | Scaffold Rust workspace | ✅ | commit `d902cea`; workspace builds, fmt+clippy `-D warnings` clean |
| 2 | Recording via cpal at 192 kHz/32-f | ✅ | `recorder.rs`: capability negotiation prefers F32@192k, falls back honestly (`rate_degraded()`); disk-streamed via lock-free ring → RAM flat; commit `a8b7670` |
| 3 | WAV import/export via hound | ✅ | `wav.rs`: PCM 8/16/24/32-bit + float32 in, float32/24-bit out; commit `4d61f28` |
| 4 | MP3 import/export (symphonia + LAME) | ✅ | `mp3.rs`: CBR 32–320 kbps + VBR V0–V9, IEEE-f32 direct feed, Xing tag splicing, 192 kHz→48 kHz export path; commit `d0da59a` |
| 5 | Audio player (play/pause/stop) | ✅ | `player.rs` + `transport.rs`: pause/resume with ring continuity, seek with generation-flush, lock-free position; bonus seek; commit `b8a6d82` |
| 6 | Unit tests for I/O round-trips | ✅ | 48 tests green (42 in `mvl-io`, 6 in `mvl-core`), incl. bit-exact WAV round-trips and lag-aligned MP3 SNR ≥ 25 dB @ 320 kbps |
| 7 | Commit+push after each sub-item | ✅ | 5 commits, all pushed to `main` |

## 3. Engineering decisions worth recording

1. **symphonia pinned to 0.5.5.** The 2026-republished symphonia 0.6.x
   line turned out to be a v2 development preview — its own crate docs
   state *"SemVer compatibility is not guaranteed … never use in any
   production application"*, and its API has already diverged
   (`symphonia::core::probe` moved, registry model rewritten). The
   classic, battle-tested line is 0.5.5 (2025-10-11). We pin 0.5.5 with a
   comment in `Cargo.toml`. This is a deliberate downgrade-for-stability.
2. **LAME output-buffer contract.** `mp3lame_encoder`'s `encode_to_vec` /
   `flush_to_vec` write through LAME's C API directly into the `Vec`'s
   *spare capacity*. Failing to `reserve(max_required_buffer_size(n))`
   **before every call** makes LAME write out of bounds — our first test
   run segfaulted, which is exactly the class of bug the round-trip test
   suite exists to catch. Fixed; the contract is documented in-code.
3. **cpal 0.18 API shifts** (vs. pre-2026 docs): `SampleRate` is now a
   plain `u32` type alias, device names come from `Display` instead of
   `name()`, `supported_input_configs()` returns an iterator, and
   `build_input_stream` takes `StreamConfig` by value. All absorbed.
4. **Environment note (CI-relevant):** the build container lacks
   `libasound2-dev` and has no sudo; ALSA headers + a `.pc` file were
   installed into a user prefix (`~/.local/alsa-dev`) linked against the
   system `libasound.so.2`, with `PKG_CONFIG_PATH` set at build time.
   GitHub Actions runners install `libasound2-dev` normally (Phase 5).
5. **No real audio hardware in this environment.** The container exposes
   an ALSA "Default Audio Device" alias that resolves to no card. All
   hardware-touching tests are `#[ignore]`-gated and fail with clean
   typed errors here; they run on demand on dev machines (Phase 5
   verification).

## 4. Test inventory (48 green, 2 hardware-ignored)

- **mvl-core (6):** neutral-identity params, clamping, NaN rejection,
  equal-temperament ratios, formant-ratio direction, profile shapes.
- **WAV (8):** bit-exact f32 round-trips (mono 48k, stereo 192k, extreme
  values incl. denormals), 24-bit quantization bound, foreign i16/i8
  files (8-bit bias trap), RIFF sniff, empty file.
- **Resample (6):** identity clone, duration preservation ±1 frame
  (192k→48k), 440 Hz tone survival via Goertzel (100× neighbor
  rejection), stereo separation, NaN rejection, empty passthrough.
- **MP3 (7):** rate mapping, mono 320 kbps round-trip (lag-aligned SNR >
  25 dB), stereo image preservation, VBR decode, 192 kHz session →
  48 kHz export, 3-channel rejection, magic sniff.
- **Recorder (9 + 1 hw):** negotiation matrix — exact 192k F32, highest
  fallback rate, F32-beats-I16, I16-only, channel preference, exact-rate
  priority, telephony rejection, empty, rate cap.
- **Player (5 + 1 hw):** sink play/pause/resume continuity, seek flush,
  consumption accounting, mono→stereo upmix, stereo→mono cancellation,
  rate adaptation.

## 5. Quality gates

- `cargo fmt --check`: clean.
- `cargo clippy --workspace --all-targets -- -D warnings`: clean.
- `cargo test --workspace`: 48 passed, 0 failed, 2 ignored (hardware).
- No `unsafe` in any of our crates (all `unsafe` is confined inside the
  audited third-party crates: LAME bindings, rtrb).
- Release build succeeds; binary **355 KB** (budget < 50 MB — huge
  headroom for the Slint UI in Phase 4).

## 6. Honest gaps (carried forward)

1. **Hardware verification pending** — recording/playback smoke tests
   are `#[ignore]`d until run on real hardware (Phase 5 protocol
   includes exactly this on the dev machine with artifacts + evidence).
2. **mvl-app is a self-check stub** — by protocol the UI is Phase 4.
3. **Player resamples up front** — fine for typical vocal clips; the
   streamed preview path (plan §6.6) arrives with the DSP engine in
   Phase 3.
4. **WAV 64-bit float import rejected loudly** (hound limitation,
   vanishingly rare format); symphonia fallback also declines; error
   message is explicit rather than silent corruption.

## 7. Path to Phase 3 (DSP engine)

The I/O layer is now a stable substrate: `InterleavedAudio` in, engine
out, `InterleavedAudio` back out. Phase 3 implements `mvl-core`'s
processing chain (analysis layer, phase-locked PV pitch, true-envelope
formant, air/breath engine, preview profile) against the invariants of
plan §6.7, with golden-signal tests and before/after samples.
