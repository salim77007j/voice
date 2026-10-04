# Micro-Vocal Lab

[![CI](https://github.com/salim77007j/voice/actions/workflows/ci.yml/badge.svg)](https://github.com/salim77007j/voice/actions/workflows/ci.yml)

**Precision voice surgery, free and open.** A standalone, Rust-native desktop application
(Windows · macOS · Linux) for microscopic control over the recorded human voice.

Micro-Vocal Lab decomposes a vocal recording into its natural components — **pitch**,
**breath/air**, and **formants (vocal-tract shape)** — and lets you reshape each one with
professional precision while preserving natural timing and articulation:

- **Pitch Shift** — ±12 semitones at 1-cent resolution. Formant-preserving (no chipmunk
  effect), timing-exact (duration is bit-identical to the source).
- **Air & Breath Control** — add warm airiness or remove breaths and sibilance with
  0.1 dB precision, without touching clarity or articulation.
- **Formant Shifter** — reshape the perceived vocal tract (100–260 mm: child → large
  adult) while pitch and intelligibility stay 100 % intact.

Plus: 192 kHz / 32-bit float recording, WAV/MP3 import, lossless WAV + configurable-bitrate
MP3 export (non-destructive), sub-millisecond waveform zoom, a professional dark studio UI
in English with optional Arabic (RTL) localization, and real-time preview under 20 ms.

## Status

| Phase | Scope | State |
|---|---|---|
| 1 | Research + architecture plan | ✅ done — see [`docs/PHASE_1_REPORT.md`](docs/PHASE_1_REPORT.md) |
| 2 | Scaffold + audio I/O (record, import, export, playback) | ✅ done — see [`docs/PHASE_2_REPORT.md`](docs/PHASE_2_REPORT.md) |
| 3 | DSP engine (pitch / air / formant) | ✅ done — see [`docs/PHASE_3_REPORT.md`](docs/PHASE_3_REPORT.md) |
| 4 | UI (Slint, EN + RTL) | ✅ done — see [`docs/PHASE_4_REPORT.md`](docs/PHASE_4_REPORT.md) + [`docs/phase4-screenshots/`](docs/phase4-screenshots/) |
| 5 | CI matrix + on-device verification kit + size pass | ✅ done — see [`docs/PHASE_5_REPORT.md`](docs/PHASE_5_REPORT.md) + [`docs/ONDEVICE.md`](docs/ONDEVICE.md) |
| 6 | Final validation + v1.0.0 | ✅ done — see [`docs/PHASE_6_REPORT.md`](docs/PHASE_6_REPORT.md) + [`docs/phase6-evidence/`](docs/phase6-evidence/) |

**v1.0.0** — CI green on all 5 OS legs (tag run: 10/10 jobs); 160 tests; 22.7× realtime render; preview 10.7 ms;
cold start 36 ms; RAM ≤ 22.4 MiB; binary 21.64 MiB. Evidence pack in
[`docs/phase6-evidence/`](docs/phase6-evidence/). Prebuilt binaries (Linux/Windows/macOS, x86_64 + aarch64)
are attached as artifacts of the [v1.0.0 tag CI run](https://github.com/salim77007j/voice/actions/runs/37208950133).
On-device checklist (real mic/speakers): [`docs/ONDEVICE.md`](docs/ONDEVICE.md).

## Technology

Rust · [Slint](https://slint.dev) 1.18 (UI, Phase 4) · cpal 0.18 (audio I/O) · hound 3.5 (WAV) ·
symphonia 0.5 (MP3 decode) · LAME 3.100 bundled (MP3 encode) · rubato 5 (resampling) ·
rustfft/realfft (STFT, Phase 3). Full decision record with rejected alternatives:
[`docs/ARCHITECTURE_PLAN.md`](docs/ARCHITECTURE_PLAN.md).

## The application

```sh
cargo run --release -p mvl-app            # desktop UI (optionally: run FILE.wav)
```

A professional dark studio UI ("Precision Studio Dark", IBM Plex, teal accent):
toolbar with 192 kHz recording and import/export, transport with sub-millisecond
waveform zoom down to individual samples, the three precision modules bound
**live** to the same engine the export uses, status telemetry (applied air gain,
preview latency), and instant English ⇄ العربية switching with full RTL
mirroring. Real renders of the UI (not mockups) are committed in
[`docs/phase4-screenshots/`](docs/phase4-screenshots/).

On headless machines the same UI can be rendered to PNG for verification:

```sh
micro-vocal-lab screenshot out.png take.wav --pitch 3 --air -30 --tract 140 --locale ar
```

And on a real desktop, one command verifies the whole hardware chain and
collects the evidence (see [`docs/ONDEVICE.md`](docs/ONDEVICE.md)):

```sh
micro-vocal-lab selftest --seconds 10
```

## The engine from the command line

The binary also exposes the full DSP headlessly:

```sh
cargo run --release -p mvl-app -- process input.wav output.wav \
    --pitch 5.0 --air -40 --tract 130 --profile render
```

`--pitch` semitones (±12, 0.01 resolution) · `--air` percent (−100…+100) ·
`--tract` mm (100–260, 170 neutral) · `--profile preview|render`.
Before/after examples of every processing mode are committed in
[`samples/`](samples/) (see its README), generated from the deterministic
sources in [`testdata/`](testdata/).

## License

GPL-3.0 — see [`LICENSE`](LICENSE). Fonts: IBM Plex family (OFL-1.1).

## Development

```sh
cargo build --workspace                 # Linux needs libasound2-dev (ALSA headers)
cargo test --workspace                  # 160 tests, headless, runs everywhere
cargo test --workspace -- --ignored     # hardware smoke tests — real devices only
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

CI (`.github/workflows/ci.yml`) runs fmt + clippy, the full test suite on
**ubuntu-22.04 / ubuntu-24.04 / windows / macos (arm64 + x86_64)** — the Linux
legs additionally exercise the audio stack against a virtual ALSA null device
(`MVL_VIRTUAL_AUDIO=1`, path coverage without timing claims) — then builds
per-OS release artifacts (21.4 MB on Linux, fat LTO). Artifacts are attached
to every commit on `main` and to `v*` tags. macOS artifacts are unsigned:
`xattr -cr micro-vocal-lab` on first run.
