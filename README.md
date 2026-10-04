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
| 1 | Research + architecture plan | ✅ done — see [`docs/ARCHITECTURE_PLAN.md`](docs/ARCHITECTURE_PLAN.md) |
| 2 | Scaffold + audio I/O (record, import, export, playback) | ✅ done — see [`docs/PHASE_2_REPORT.md`](docs/PHASE_2_REPORT.md) |
| 3 | DSP engine (pitch / air / formant) | ⏳ next |
| 4 | UI (Slint, EN + RTL) | planned |
| 5 | CI + integration + testing | planned |
| 6 | Final validation + v1.0.0 | planned |

## Technology

Rust · [Slint](https://slint.dev) 1.18 (UI, Phase 4) · cpal 0.18 (audio I/O) · hound 3.5 (WAV) ·
symphonia 0.5 (MP3 decode) · LAME 3.100 bundled (MP3 encode) · rubato 5 (resampling) ·
rustfft/realfft (STFT, Phase 3). Full decision record with rejected alternatives:
[`docs/ARCHITECTURE_PLAN.md`](docs/ARCHITECTURE_PLAN.md).

## License

GPL-3.0 — see [`LICENSE`](LICENSE). Fonts: IBM Plex family (OFL-1.1).
