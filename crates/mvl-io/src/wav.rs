//! WAV import and export via [hound].
//!
//! Import covers every WAV flavour hound can read: PCM 8/16/24/32-bit and
//! IEEE float 32/64-bit, any channel count, any sample rate — all
//! normalized to interleaved `f32` in [`crate::InterleavedAudio`].
//!
//! Export writes either lossless 32-bit float (the professional default)
//! or 24-bit PCM, per the architecture plan §5.3.

use std::path::Path;

use hound::{SampleFormat, WavSpec, WavWriter};

use crate::{Error, InterleavedAudio, Result};

/// Target bit depth for WAV export.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WavDepth {
    /// 32-bit IEEE float — bit-exact for our internal format, the
    /// professional default (non-destructive of `f32` processing).
    #[default]
    Float32,
    /// 24-bit PCM integer — for interchange with tools that choke on
    /// float WAVs.
    Pcm24,
}

/// Quick content sniff: does this file start with a RIFF chunk (i.e. is it
/// plausibly a WAV)?
///
/// # Errors
/// [`Error::Io`] if the file cannot be read at all. A read that yields
/// fewer than 12 bytes simply answers `false` (not a WAV).
pub fn looks_like_wav(path: &Path) -> Result<bool> {
    use std::io::Read;
    let mut magic = [0u8; 12];
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => {
            return Err(Error::io(
                format!("open '{}' for sniffing", path.display()),
                e,
            ))
        }
    };
    let n = file
        .read(&mut magic)
        .map_err(|e| Error::io(format!("read header of '{}'", path.display()), e))?;
    if n < 12 {
        return Ok(false);
    }
    Ok(&magic[0..4] == b"RIFF" && &magic[8..12] == b"WAVE")
}

/// Import a WAV file as interleaved `f32`.
///
/// Integer formats are normalized to `−1.0 ..= 1.0` by their full signed
/// range (8-bit WAVs are unsigned per the RIFF spec; hound folds the 128
/// offset in for us). Float formats pass through (f64 is narrowed).
///
/// # Errors
/// * [`Error::Io`] — unreadable file
/// * [`Error::Wav`] — malformed WAV, unsupported bit depth, or truncated
///   sample data
pub fn import(path: &Path) -> Result<InterleavedAudio> {
    let mut reader = hound::WavReader::open(path)
        .map_err(|e| Error::Wav(format!("open '{}': {e}", path.display())))?;
    let spec = reader.spec();

    if spec.channels == 0 || spec.sample_rate == 0 {
        return Err(Error::Wav(format!(
            "'{}' declares zero channels or zero sample rate",
            path.display()
        )));
    }

    let n_samples = reader.len() as usize;
    let mut data = Vec::with_capacity(n_samples);

    match spec.sample_format {
        SampleFormat::Float => match spec.bits_per_sample {
            32 => {
                for s in reader.samples::<f32>() {
                    data.push(to_finite(s, path)?);
                }
            }
            bits => {
                // hound only reads 32-bit floats; 64-bit float WAVs are a
                // vanishing rarity and rejected loudly rather than mangled.
                return Err(Error::Wav(format!(
                    "'{}': unsupported float width ({bits} bits; expected 32)",
                    path.display()
                )));
            }
        },
        SampleFormat::Int => {
            let scale = int_scale(spec.bits_per_sample).ok_or_else(|| {
                Error::Wav(format!(
                    "'{}': unsupported integer width ({} bits)",
                    path.display(),
                    spec.bits_per_sample
                ))
            })?;
            for s in reader.samples::<i32>() {
                let raw = s.map_err(|e| Error::Wav(format!("read '{}': {e}", path.display())))?;
                data.push(raw as f32 / scale);
            }
        }
    }

    InterleavedAudio::new(data, spec.sample_rate, spec.channels)
        .map_err(|e| Error::Wav(format!("'{}': {e}", path.display())))
}

/// Export interleaved `f32` audio to WAV at the chosen depth.
///
/// Float32 output is bit-exact for finite sample values. Pcm24 quantizes
/// with rounding to the nearest of 2^24 levels and dithering is *not*
/// applied (kept deterministic for testing; a dither option can come later
/// without breaking the API).
///
/// # Errors
/// * [`Error::Io`] — unwritable target path
/// * [`Error::Wav`] — writer failure mid-stream
pub fn export(path: &Path, audio: &InterleavedAudio, depth: WavDepth) -> Result<()> {
    let spec = match depth {
        WavDepth::Float32 => WavSpec {
            channels: audio.channels,
            sample_rate: audio.sample_rate,
            bits_per_sample: 32,
            sample_format: SampleFormat::Float,
        },
        WavDepth::Pcm24 => WavSpec {
            channels: audio.channels,
            sample_rate: audio.sample_rate,
            bits_per_sample: 24,
            sample_format: SampleFormat::Int,
        },
    };

    let mut writer = WavWriter::create(path, spec)
        .map_err(|e| Error::Wav(format!("create '{}': {e}", path.display())))?;

    match depth {
        WavDepth::Float32 => {
            for &s in &audio.data {
                writer
                    .write_sample(s)
                    .map_err(|e| Error::Wav(format!("write '{}': {e}", path.display())))?;
            }
        }
        WavDepth::Pcm24 => {
            for &s in &audio.data {
                let v = (s.clamp(-1.0, 1.0) * 8_388_607.0).round() as i32;
                writer
                    .write_sample(v)
                    .map_err(|e| Error::Wav(format!("write '{}': {e}", path.display())))?;
            }
        }
    }

    writer
        .finalize()
        .map_err(|e| Error::Wav(format!("finalize '{}': {e}", path.display())))
}

/// Full-range scale factor for a signed integer PCM width, or `None` for
/// widths we do not support (anything outside 8/16/24/32).
fn int_scale(bits: u16) -> Option<f32> {
    match bits {
        8 => Some(128.0),
        16 => Some(32_768.0),
        24 => Some(8_388_608.0),
        32 => Some(2_147_483_648.0),
        _ => None,
    }
}

/// Guard against NaN/Inf sample values corrupting downstream math; those
/// are invalid in a well-formed WAV and we fail loudly instead.
fn to_finite(s: std::result::Result<f32, hound::Error>, path: &Path) -> Result<f32> {
    let v = s.map_err(|e| Error::Wav(format!("read '{}': {e}", path.display())))?;
    if v.is_finite() {
        Ok(v)
    } else {
        Err(Error::Wav(format!(
            "'{}' contains non-finite (NaN/Inf) samples",
            path.display()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Deterministic pseudo-random signal in [-1, 1) — an LCG keeps test
    /// fixtures reproducible across platforms.
    fn lcg_signal(n: usize, seed: u64) -> Vec<f32> {
        let mut state = seed;
        (0..n)
            .map(|_| {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                ((state >> 33) as i32 as f32) / (i32::MAX as f32) * 0.999_999
            })
            .collect()
    }

    fn temp(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("mvl_wav_test_{name}_{}.wav", std::process::id()));
        p
    }

    #[test]
    fn float32_roundtrip_is_bit_exact_mono_48k() {
        let path = temp("f32_mono");
        let source = InterleavedAudio::new(lcg_signal(10_000, 42), 48_000, 1).unwrap();
        export(&path, &source, WavDepth::Float32).unwrap();
        let back = import(&path).unwrap();
        assert_eq!(back.sample_rate, 48_000);
        assert_eq!(back.channels, 1);
        assert_eq!(
            back.data, source.data,
            "float WAV round-trip must be bit-exact"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn float32_roundtrip_is_bit_exact_stereo_192k() {
        let path = temp("f32_stereo_192k");
        let raw = lcg_signal(48_000, 7); // 0.125 s stereo at 192 kHz
        let source = InterleavedAudio::new(raw, 192_000, 2).unwrap();
        export(&path, &source, WavDepth::Float32).unwrap();
        let back = import(&path).unwrap();
        assert_eq!(back.sample_rate, 192_000);
        assert_eq!(back.channels, 2);
        assert_eq!(back.data, source.data);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn float32_roundtrip_survives_extremes() {
        let path = temp("f32_extremes");
        let source = InterleavedAudio::new(
            vec![
                0.0,
                1.0,
                -1.0,
                0.5,
                -0.5,
                f32::MIN_POSITIVE,
                1e-20,
                -1e-20,
                0.999_999_94,
            ],
            44_100,
            1,
        )
        .unwrap();
        export(&path, &source, WavDepth::Float32).unwrap();
        let back = import(&path).unwrap();
        assert_eq!(back.data, source.data);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn pcm24_roundtrip_within_quantization_bound() {
        let path = temp("i24");
        let source = InterleavedAudio::new(lcg_signal(5_000, 99), 96_000, 2).unwrap();
        export(&path, &source, WavDepth::Pcm24).unwrap();
        let back = import(&path).unwrap();
        assert_eq!(back.sample_rate, 96_000);
        assert_eq!(back.channels, 2);
        let bound = 1.0 / 8_388_607.0 + 1e-9;
        for (a, b) in source.data.iter().zip(&back.data) {
            assert!(
                (a - b).abs() <= bound,
                "24-bit quantization error too large: {a} vs {b}"
            );
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn imports_foreign_i16_wav_normalized() {
        let path = temp("foreign_i16");
        let spec = WavSpec {
            channels: 1,
            sample_rate: 22_050,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };
        let values = [-32_768i32, -16_384, -1, 0, 1, 16_383, 32_767];
        let mut w = WavWriter::create(&path, spec).unwrap();
        for v in values {
            w.write_sample(v).unwrap();
        }
        w.finalize().unwrap();

        let back = import(&path).unwrap();
        assert_eq!(back.sample_rate, 22_050);
        for (raw, f) in values.iter().zip(&back.data) {
            let expected = *raw as f32 / 32_768.0;
            assert!((expected - f).abs() < 1e-9);
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn imports_foreign_i8_wav_normalized() {
        // 8-bit WAV is unsigned with a 128 bias — a classic interop trap.
        let path = temp("foreign_i8");
        let spec = WavSpec {
            channels: 1,
            sample_rate: 8_000,
            bits_per_sample: 8,
            sample_format: SampleFormat::Int,
        };
        let values = [-128i32, 0, 64, 127];
        let mut w = WavWriter::create(&path, spec).unwrap();
        for v in values {
            w.write_sample(v).unwrap();
        }
        w.finalize().unwrap();

        let back = import(&path).unwrap();
        for (raw, f) in values.iter().zip(&back.data) {
            let expected = *raw as f32 / 128.0;
            assert!(
                (expected - f).abs() < 1e-9,
                "8-bit bias handling: {raw} -> {f}"
            );
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn sniffs_wav_magic() {
        let path = temp("sniff");
        let source = InterleavedAudio::new(vec![0.0; 16], 48_000, 1).unwrap();
        export(&path, &source, WavDepth::Float32).unwrap();
        assert!(looks_like_wav(&path).unwrap());

        let mut garbage = temp("garbage");
        garbage.set_extension("bin");
        std::fs::write(&garbage, b"not a riff file at all").unwrap();
        assert!(!looks_like_wav(&garbage).unwrap());
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&garbage);
    }

    #[test]
    fn empty_audio_exports_valid_wav() {
        let path = temp("empty");
        let source = InterleavedAudio::new(Vec::new(), 48_000, 1).unwrap();
        export(&path, &source, WavDepth::Float32).unwrap();
        let back = import(&path).unwrap();
        assert_eq!(back.frames(), 0);
        let _ = std::fs::remove_file(&path);
    }
}
