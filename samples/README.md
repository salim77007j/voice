# Micro-Vocal Lab — before/after sample pack

All renders: 48 kHz mono float WAV, Render profile (2048-pt STFT).
Source signals live in `testdata/` (same repo, deterministic).
Regenerate with `cargo test -p mvl-io -- --ignored`.

| File | Source & processing |
|---|---|
| `neutral_bypass.wav` | voiced_with_breath_tail.wav (params neutral — engine bit-exact bypass, invariant #1) |
| `pitch_plus_7st.wav` | voiced_harmonic_stack_220hz.wav (pitch +7.00 st) |
| `pitch_minus_5st_vowel.wav` | vowel_a_196hz.wav (pitch -5.00 st) |
| `formant_short_tract_120mm.wav` | vowel_a_196hz.wav (tract 120 mm — shorter tract, formants up) |
| `formant_long_tract_230mm.wav` | vowel_a_196hz.wav (tract 230 mm — longer tract, formants down) |
| `air_removal_minus_100.wav` | voiced_with_breath_tail.wav (air -100 % — breath ducked, voice untouched) |
| `air_add_plus_70.wav` | vowel_sequence_aiu.wav (air +70 % — shelf + harmonic air) |
| `combined_pitch5_tract130_air_minus40.wav` | vowel_sequence_aiu.wav (pitch +5.00 st, tract 130 mm, air -40 %) |
