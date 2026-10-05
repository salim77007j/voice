//! Golden fixtures: `testdata/` source signals and `samples/`
//! before/after renders (plan §10.2 / §10.6).
//!
//! The generator tests are `#[ignore]`d — run
//! `cargo test -p mvl-io -- --ignored` to regenerate the committed
//! files after changing a generator or the engine. The non-ignored
//! tests verify the committed files exist, decode, and (for the
//! samples) that the engine reproduces them bit-exactly on the
//! generating platform, within a tight cross-platform tolerance
//! elsewhere (see `engine_reproduces_committed_samples`).

use mvl_core::testsupport as ts;
use mvl_core::{QualityProfile, VocalEngine, VocalParams};
use mvl_io::wav;
use mvl_io::InterleavedAudio;

const RATE: u32 = 48_000;

fn write_wav(path: &std::path::Path, data: &[f32]) {
    let audio = InterleavedAudio::new(data.to_vec(), RATE, 1)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    wav::export(path, &audio, mvl_io::WavDepth::Float32)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    println!("wrote {} ({} samples)", path.display(), data.len());
}

fn read_wav(path: &std::path::Path) -> InterleavedAudio {
    wav::import(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn repo_root() -> std::path::PathBuf {
    // tests run with CWD = crate dir (crates/mvl-io)
    let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p
}

// ---------------------------------------------------------------------
// Generators (run with --ignored to regenerate the committed files)
// ---------------------------------------------------------------------

/// The committed source signals. Deterministic by construction (seeded
/// noise, fixed-phase synthesis).
fn source_signals() -> Vec<(&'static str, Vec<f32>)> {
    let vowels = |f0: f64, f1: f64, f2: f64, secs: f64| {
        ts::formant_vowel(
            f0,
            &[(f1, 10.0), (f2, 10.0), (2500.0, 12.0)],
            0.5,
            (secs * f64::from(RATE)) as usize,
            RATE,
        )
    };
    let stack = ts::harmonic_stack(220.0, 12, 0.5, 96_000, RATE);
    let breath = ts::breath_noise(101, 48_000, RATE, 0.063);
    let sib = ts::sibilant_noise(102, 48_000, RATE, 0.1);
    let voiced_breathy = {
        // A voiced phrase with a breath tail: the material the air
        // engine is actually for.
        let vowel = ts::fade_in(vowels(196.0, 730.0, 1090.0, 1.0), 5.0, RATE);
        let breath_tail = ts::breath_noise(103, 24_000, RATE, 0.05);
        ts::concat(&vowel, &breath_tail)
    };
    let vowel_sequence = {
        // /a/ -> /i/ -> /u/: three formant settings the formant
        // engine must warp coherently.
        let a = vowels(196.0, 730.0, 1090.0, 0.5);
        let i = vowels(196.0, 270.0, 2290.0, 0.5);
        let u = vowels(196.0, 300.0, 870.0, 0.5);
        let mut seq = ts::concat(&a, &i);
        seq.extend_from_slice(&u);
        seq
    };
    vec![
        ("voiced_harmonic_stack_220hz.wav", stack),
        ("vowel_a_196hz.wav", vowels(196.0, 730.0, 1090.0, 1.5)),
        ("breath_noise.wav", breath),
        ("sibilant_noise.wav", sib),
        ("voiced_with_breath_tail.wav", voiced_breathy),
        ("vowel_sequence_aiu.wav", vowel_sequence),
        ("silence_500ms.wav", ts::silence(24_000)),
    ]
}

/// The committed before/after renders (Render profile — export quality).
fn sample_renders() -> Vec<(&'static str, String, Vec<f32>, VocalParams)> {
    let stack = ts::harmonic_stack(220.0, 12, 0.5, 96_000, RATE);
    let vowel = ts::formant_vowel(
        196.0,
        &[(730.0, 10.0), (1090.0, 10.0), (2500.0, 12.0)],
        0.5,
        72_000,
        RATE,
    );
    let phrase = source_signal_by_name("voiced_with_breath_tail.wav");
    let seq = source_signal_by_name("vowel_sequence_aiu.wav");
    let vowel_a = source_signal_by_name("vowel_a_196hz.wav");
    vec![
        (
            "neutral_bypass.wav",
            "voiced_with_breath_tail.wav (params neutral — engine bit-exact bypass, invariant #1)"
                .into(),
            phrase.clone(),
            VocalParams::neutral(),
        ),
        (
            "pitch_plus_7st.wav",
            "voiced_harmonic_stack_220hz.wav (pitch +7.00 st)".into(),
            stack,
            VocalParams {
                pitch_semitones: 7.0,
                ..VocalParams::neutral()
            },
        ),
        (
            "pitch_minus_5st_vowel.wav",
            "vowel_a_196hz.wav (pitch -5.00 st)".into(),
            vowel,
            VocalParams {
                pitch_semitones: -5.0,
                ..VocalParams::neutral()
            },
        ),
        (
            "formant_short_tract_120mm.wav",
            "vowel_a_196hz.wav (tract 120 mm — shorter tract, formants up)".into(),
            vowel_a.clone(),
            VocalParams {
                tract_mm: 120.0,
                ..VocalParams::neutral()
            },
        ),
        (
            "formant_long_tract_230mm.wav",
            "vowel_a_196hz.wav (tract 230 mm — longer tract, formants down)".into(),
            vowel_a,
            VocalParams {
                tract_mm: 230.0,
                ..VocalParams::neutral()
            },
        ),
        (
            "air_removal_minus_100.wav",
            "voiced_with_breath_tail.wav (air -100 % — breath ducked, voice untouched)".into(),
            phrase,
            VocalParams {
                air_percent: -100,
                ..VocalParams::neutral()
            },
        ),
        (
            "air_add_plus_70.wav",
            "vowel_sequence_aiu.wav (air +70 % — shelf + harmonic air)".into(),
            seq,
            VocalParams {
                air_percent: 70,
                ..VocalParams::neutral()
            },
        ),
        (
            "combined_pitch5_tract130_air_minus40.wav",
            "vowel_sequence_aiu.wav (pitch +5.00 st, tract 130 mm, air -40 %)".into(),
            source_signal_by_name("vowel_sequence_aiu.wav"),
            VocalParams {
                pitch_semitones: 5.0,
                tract_mm: 130.0,
                air_percent: -40,
                ..VocalParams::neutral()
            },
        ),
    ]
}

fn source_signal_by_name(name: &str) -> Vec<f32> {
    source_signals()
        .into_iter()
        .find(|(n, _)| *n == name)
        .map(|(_, d)| d)
        .unwrap_or_else(|| panic!("unknown source {name}"))
}

#[test]
#[ignore = "regenerates committed fixtures — run explicitly"]
fn regenerate_testdata() {
    let dir = repo_root().join("testdata");
    std::fs::create_dir_all(&dir).expect("create testdata dir");
    for (name, data) in source_signals() {
        write_wav(&dir.join(name), &data);
    }
}

#[test]
#[ignore = "regenerates committed fixtures — run explicitly"]
fn regenerate_samples() {
    let dir = repo_root().join("samples");
    std::fs::create_dir_all(&dir).expect("create samples dir");
    let mut readme = String::from(
        "# Micro-Vocal Lab — before/after sample pack\n\n\
         All renders: 48 kHz mono float WAV, Render profile (2048-pt STFT).\n\
         Source signals live in `testdata/` (same repo, deterministic).\n\
         Regenerate with `cargo test -p mvl-io -- --ignored`.\n\n\
         | File | Source & processing |\n|---|---|\n",
    );
    for (name, desc, source, params) in sample_renders() {
        let rendered = VocalEngine::render(&source, RATE, params, QualityProfile::Render)
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        write_wav(&dir.join(name), &rendered.output);
        readme.push_str(&format!("| `{name}` | {desc} |\n"));
    }
    std::fs::write(dir.join("README.md"), readme).expect("write samples README");
}

// ---------------------------------------------------------------------
// Verification of the committed files
// ---------------------------------------------------------------------

#[test]
fn testdata_files_exist_and_decode() {
    let dir = repo_root().join("testdata");
    let names = [
        "voiced_harmonic_stack_220hz.wav",
        "vowel_a_196hz.wav",
        "breath_noise.wav",
        "sibilant_noise.wav",
        "voiced_with_breath_tail.wav",
        "vowel_sequence_aiu.wav",
        "silence_500ms.wav",
    ];
    for name in names {
        let audio = read_wav(&dir.join(name));
        assert_eq!(audio.sample_rate, RATE, "{name}");
        assert_eq!(audio.channels, 1, "{name}");
        assert!(!audio.data.is_empty(), "{name} empty");
        assert!(
            audio.data.iter().all(|v| v.is_finite()),
            "{name} non-finite"
        );
    }
    // Known-content spot checks.
    let stack = read_wav(&dir.join("voiced_harmonic_stack_220hz.wav"));
    let (f0, clarity) = mvl_core::analysis::yin_f0(&stack.data, RATE).expect("stack f0");
    assert!((f0 - 220.0).abs() < 2.0, "stack f0 {f0}");
    assert!(clarity > 0.9);
}

#[test]
fn engine_reproduces_committed_samples() {
    // The engine is deterministic on a fixed platform: re-rendering the
    // committed before/after samples reproduces them sample-for-sample on
    // the platform that generated them (linux/x86_64 dev container).
    // Across platforms rustfft may select different butterfly
    // implementations (SSE/AVX/FMA on x86, NEON on aarch64), so last bits
    // can drift. Two tiers keep the regression net honest:
    //   * tier 1 — bit-exact (any CPU whose FFT dispatch matches the
    //     generating platform);
    //   * tier 2 — every sample within 1e-4 (−80 dBFS) of the committed
    //     render. A real algorithm change moves samples by orders of
    //     magnitude more than FFT-dispatch ulp drift, so regressions
    //     still fail loudly.
    let dir = repo_root().join("samples");
    for (name, _desc, source, params) in sample_renders() {
        let committed = read_wav(&dir.join(name));
        let rendered = VocalEngine::render(&source, RATE, params, QualityProfile::Render)
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(
            rendered.output.len(),
            committed.data.len(),
            "{name}: length changed"
        );
        let mut mismatches = 0usize;
        let mut max_abs_diff = 0.0f32;
        for (a, b) in rendered.output.iter().zip(&committed.data) {
            if a.to_bits() != b.to_bits() {
                mismatches += 1;
                max_abs_diff = max_abs_diff.max((a - b).abs());
            }
        }
        if mismatches > 0 {
            assert!(
                max_abs_diff <= 1e-4,
                "{name}: {mismatches} samples differ from the committed render, \
                 max |diff| {max_abs_diff:e} exceeds the 1e-4 cross-platform \
                 FFT-dispatch tolerance"
            );
            eprintln!(
                "{name}: {mismatches} samples differ by at most {max_abs_diff:e} \
                 (cross-platform FFT-dispatch drift, within tolerance)"
            );
        }
    }
}

#[test]
fn neutral_sample_is_bit_exact_bypass() {
    let dir = repo_root().join("samples");
    let neutral = read_wav(&dir.join("neutral_bypass.wav"));
    let source = read_wav(&repo_root().join("testdata/voiced_with_breath_tail.wav"));
    assert_eq!(
        neutral.data, source.data,
        "neutral render must equal source"
    );
}
