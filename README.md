# Micro-Vocal Lab

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
| 5 | CI + integration + testing | ⏳ next |
| 6 | Final validation + v1.0.0 | planned |

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
