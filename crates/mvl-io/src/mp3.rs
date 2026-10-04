//! MP3 import (symphonia) and export (LAME 3.100, bundled + statically
//! linked via [mp3lame_encoder]).
//!
//! Export feeds IEEE-float samples straight to LAME (no intermediate i16
//! quantization), at any of the LAME-legal sample rates. Sessions at
//! higher rates (e.g. our 192 kHz recordings) are resampled to 48 kHz by
//! [`crate::resample`] first — MP3 physically tops out at 48 kHz.

use std::path::Path;

use mp3lame_encoder::{
    Bitrate as LameBitrate, Builder, DualPcm, FlushNoGap, MonoPcm, Quality as LameQuality, VbrMode,
};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use crate::{Error, InterleavedAudio, Result};

/// CBR bitrate selection for MP3 export.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mp3Bitrate {
    /// 32 kbps
    Kbps32,
    /// 64 kbps
    Kbps64,
    /// 96 kbps
    Kbps96,
    /// 128 kbps
    Kbps128,
    /// 160 kbps
    Kbps160,
    /// 192 kbps (default)
    #[default]
    Kbps192,
    /// 256 kbps
    Kbps256,
    /// 320 kbps (maximum CBR quality)
    Kbps320,
}

impl Mp3Bitrate {
    fn to_lame(self) -> LameBitrate {
        match self {
            Self::Kbps32 => LameBitrate::Kbps32,
            Self::Kbps64 => LameBitrate::Kbps64,
            Self::Kbps96 => LameBitrate::Kbps96,
            Self::Kbps128 => LameBitrate::Kbps128,
            Self::Kbps160 => LameBitrate::Kbps160,
            Self::Kbps192 => LameBitrate::Kbps192,
            Self::Kbps256 => LameBitrate::Kbps256,
            Self::Kbps320 => LameBitrate::Kbps320,
        }
    }

    /// Kilobits per second, for UI display.
    pub const fn kbps(self) -> u32 {
        match self {
            Self::Kbps32 => 32,
            Self::Kbps64 => 64,
            Self::Kbps96 => 96,
            Self::Kbps128 => 128,
            Self::Kbps160 => 160,
            Self::Kbps192 => 192,
            Self::Kbps256 => 256,
            Self::Kbps320 => 320,
        }
    }
}

/// VBR quality preset (V0 = best/largest .. V9 = smallest).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VbrQuality {
    /// V0 — highest quality VBR
    V0,
    /// V1
    V1,
    /// V2 — transparent for most material
    V2,
    /// V3
    V3,
    /// V4
    V4,
    /// V5
    V5,
    /// V6
    V6,
    /// V7
    V7,
    /// V8
    V8,
    /// V9 — smallest files
    V9,
}

impl VbrQuality {
    fn to_lame(self) -> LameQuality {
        match self {
            Self::V0 => LameQuality::Best,
            Self::V1 => LameQuality::SecondBest,
            Self::V2 => LameQuality::NearBest,
            Self::V3 => LameQuality::VeryNice,
            Self::V4 => LameQuality::Nice,
            Self::V5 => LameQuality::Good,
            Self::V6 => LameQuality::Decent,
            Self::V7 => LameQuality::Ok,
            Self::V8 => LameQuality::SecondWorst,
            Self::V9 => LameQuality::Worst,
        }
    }
}

/// MP3 encoder configuration: CBR at [`Mp3Bitrate`] (default), or VBR at
/// [`VbrQuality`] when `vbr` is `Some`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Mp3Settings {
    /// CBR bitrate (ignored in VBR mode).
    pub bitrate: Mp3Bitrate,
    /// VBR preset, or `None` for CBR.
    pub vbr: Option<VbrQuality>,
}

/// Sample rates LAME can encode (MPEG-1/2/2.5 Layer III legal set).
const LAME_RATES: [u32; 9] = [
    8_000, 11_025, 12_000, 16_000, 22_050, 24_000, 32_000, 44_100, 48_000,
];

/// Map a session sample rate onto the nearest LAME-legal rate.
///
/// Rates above 48 kHz (e.g. 96/192 kHz sessions) always map to 48 kHz —
/// the maximum the MP3 format supports. For sub-48 kHz rates the nearest
/// legal rate wins.
pub fn mp3_target_rate(rate: u32) -> u32 {
    if rate >= 48_000 {
        return 48_000;
    }
    *LAME_RATES
        .iter()
        .min_by_key(|&&r| r.abs_diff(rate))
        .unwrap_or(&48_000)
}

/// LAME runtime version string (shown in the About dialog).
pub fn lame_version() -> String {
    mp3lame_encoder::mp3lame_version().into_owned()
}

/// Quick content sniff: ID3v2 magic or an MPEG audio frame sync.
///
/// # Errors
/// [`Error::Io`] if the file cannot be read at all.
pub fn looks_like_mp3(path: &Path) -> Result<bool> {
    use std::io::Read;
    let mut magic = [0u8; 4];
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
    if n < 2 {
        return Ok(false);
    }
    if n >= 3 && &magic[0..3] == b"ID3" {
        return Ok(true);
    }
    // MPEG frame sync: eleven 1-bits (0xFF followed by 0xE0 mask).
    Ok(magic[0] == 0xFF && (magic[1] & 0xE0) == 0xE0)
}

/// Import an MP3 file via symphonia, decoding to interleaved `f32` at the
/// file's native sample rate.
///
/// # Errors
/// * [`Error::Io`] — unreadable file
/// * [`Error::Mp3Decode`] — malformed stream / decode failure
pub fn import(path: &Path) -> Result<InterleavedAudio> {
    let (audio, _) = decode_with_symphonia(path)?;
    Ok(audio)
}

/// Generic symphonia import for any format its features cover (MP3, WAV,
/// …). Used as a fallback when the primary WAV path (hound) rejects an
/// exotic-but-valid RIFF dialect.
///
/// Returns the audio plus the codec's human-readable name.
///
/// # Errors
/// * [`Error::Mp3Decode`] — probing/decoding failure (message carries the
///   actual codec error)
pub fn import_any(path: &Path) -> Result<(InterleavedAudio, String)> {
    decode_with_symphonia(path)
}

/// Shared symphonia pipeline: probe -> first audio track -> decode all
/// packets -> interleaved f32.
fn decode_with_symphonia(path: &Path) -> Result<(InterleavedAudio, String)> {
    let file = std::fs::File::open(path)
        .map_err(|e| Error::io(format!("open '{}'", path.display()), e))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| Error::Mp3Decode(format!("probe '{}': {e}", path.display())))?;
    let mut format = probed.format;

    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .cloned()
        .ok_or_else(|| Error::Mp3Decode(format!("'{}' contains no audio track", path.display())))?;
    let track_id = track.id;
    let codec_name = track.codec_params.codec.to_string();

    let sample_rate = track
        .codec_params
        .sample_rate
        .ok_or_else(|| Error::Mp3Decode(format!("'{}': missing sample rate", path.display())))?;
    let channels = track
        .codec_params
        .channels
        .map(|c| c.count())
        .ok_or_else(|| Error::Mp3Decode(format!("'{}': missing channel layout", path.display())))?;

    if sample_rate == 0 || channels == 0 {
        return Err(Error::Mp3Decode(format!(
            "'{}' declares zero rate or channels",
            path.display()
        )));
    }

    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| Error::Mp3Decode(format!("open decoder for '{}': {e}", path.display())))?;

    let mut data: Vec<f32> = Vec::new();
    let mut sample_buf: Option<SampleBuffer<f32>> = None;

    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymphoniaError::IoError(ref e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break
            }
            Err(SymphoniaError::ResetRequired) => {
                return Err(Error::Mp3Decode(format!(
                    "'{}': stream reset mid-decode",
                    path.display()
                )));
            }
            Err(e) => return Err(Error::Mp3Decode(format!("read '{}': {e}", path.display()))),
        };
        if packet.track_id() != track_id {
            continue;
        }

        let decoded = decoder
            .decode(&packet)
            .map_err(|e| Error::Mp3Decode(format!("decode '{}': {e}", path.display())))?;

        // (Re)allocate the interleaving scratch buffer if this packet is
        // larger than any before it.
        let capacity = decoded.capacity() as u64;
        let spec_now = *decoded.spec();
        if sample_buf.is_none()
            || sample_buf
                .as_ref()
                .is_some_and(|b| b.capacity() < capacity as usize)
        {
            sample_buf = Some(SampleBuffer::new(capacity, spec_now));
        }
        let buf = sample_buf.as_mut().expect("just ensured");
        buf.copy_interleaved_ref(decoded);
        data.extend_from_slice(buf.samples());
    }

    let audio = InterleavedAudio::new(data, sample_rate, channels as u16)
        .map_err(|e| Error::Mp3Decode(format!("'{}': {e}", path.display())))?;
    Ok((audio, codec_name))
}

/// Encode frames per `encode_to_vec` call — a multiple of MP3's 1152-sample
/// granule keeps LAME's internals happy and the output vec bounded.
const ENCODE_CHUNK_FRAMES: usize = 1152 * 8;

/// Export interleaved audio to an MP3 file.
///
/// * Channels: mono or stereo (LAME's limit — >2 is an [`Error::Mp3Encode`]).
/// * Sample rate: any; non-LAME-legal rates are resampled via
///   [`crate::resample`] (192 kHz sessions land at 48 kHz, the MP3 maximum).
/// * Non-destructive: writes a new file, never touches the source.
///
/// # Errors
/// * [`Error::Mp3Encode`] — encoder construction or encode failure
/// * [`Error::Resample`] — rate conversion failure
/// * [`Error::Io`] — unwritable target
pub fn export(path: &Path, audio: &InterleavedAudio, settings: &Mp3Settings) -> Result<()> {
    if audio.channels > 2 {
        return Err(Error::Mp3Encode(format!(
            "MP3 supports at most 2 channels (got {})",
            audio.channels
        )));
    }
    if audio.data.iter().any(|s| !s.is_finite()) {
        return Err(Error::Mp3Encode(
            "refusing to encode non-finite (NaN/Inf) samples".into(),
        ));
    }

    // Bring the audio onto a LAME-legal rate (resample clones/converts as
    // needed; identity rate is a cheap clone).
    let target_rate = mp3_target_rate(audio.sample_rate);
    let source = crate::resample::resample(audio, target_rate)?;

    let mut builder =
        Builder::new().ok_or_else(|| Error::Mp3Encode("LAME builder init failed".into()))?;
    builder = builder
        .with_num_channels(source.channels as u8)
        .map_err(|e| Error::Mp3Encode(format!("set channels: {e}")))?
        .with_sample_rate(target_rate)
        .map_err(|e| Error::Mp3Encode(format!("set sample rate {target_rate}: {e}")))?;

    builder = match settings.vbr {
        Some(v) => builder
            .with_vbr_mode(VbrMode::Mtrh)
            .map_err(|e| Error::Mp3Encode(format!("set VBR mode: {e}")))?
            .with_vbr_quality(v.to_lame())
            .map_err(|e| Error::Mp3Encode(format!("set VBR quality: {e}")))?
            .with_to_write_vbr_tag(true)
            .map_err(|e| Error::Mp3Encode(format!("enable VBR tag: {e}")))?,
        None => builder
            .with_brate(settings.bitrate.to_lame())
            .map_err(|e| Error::Mp3Encode(format!("set CBR bitrate: {e}")))?,
    };

    let mut encoder = builder
        .build()
        .map_err(|e| Error::Mp3Encode(format!("LAME init: {e}")))?;

    let mut mp3: Vec<u8> = Vec::new();
    let channels = source.channels as usize;

    for chunk in source.data.chunks(ENCODE_CHUNK_FRAMES * channels) {
        // CRITICAL: encode_to_vec writes through LAME's C API into the
        // Vec's *spare capacity* — reserve before every call, or LAME
        // writes out of bounds (SIGSEGV).
        mp3.reserve(mp3lame_encoder::max_required_buffer_size(chunk.len()));
        if channels == 1 {
            encoder
                .encode_to_vec(MonoPcm(chunk), &mut mp3)
                .map_err(|e| Error::Mp3Encode(format!("encode: {e}")))?;
        } else {
            let (left, right) = deinterleave(chunk);
            mp3.reserve(mp3lame_encoder::max_required_buffer_size(left.len()));
            encoder
                .encode_to_vec(
                    DualPcm {
                        left: &left,
                        right: &right,
                    },
                    &mut mp3,
                )
                .map_err(|e| Error::Mp3Encode(format!("encode: {e}")))?;
        }
    }

    // Flush needs >= 7200 bytes of headroom (LAME contract).
    mp3.reserve(mp3lame_encoder::max_required_buffer_size(0) + 8_192);
    encoder
        .flush_to_vec::<FlushNoGap>(&mut mp3)
        .map_err(|e| Error::Mp3Encode(format!("flush: {e}")))?;

    // VBR streams carry a LAME/Xing header frame (seek/duration info).
    // LAME wrote the tag *placeholder* into the stream; splice the real
    // tag bytes in right after the (here absent) ID3v2 block.
    let tag_size = encoder.lame_tag_size();
    if tag_size > 0 {
        let id3_boundary = encoder.id3v2_tag_size();
        let mut tag = Vec::with_capacity(tag_size);
        if encoder.lame_tag_encode_to_vec(&mut tag).is_some() {
            let boundary = id3_boundary.min(mp3.len());
            mp3.splice(boundary..boundary, tag);
        }
    }

    std::fs::write(path, mp3).map_err(|e| Error::io(format!("write '{}'", path.display()), e))
}

/// Split an interleaved stereo slice into (left, right).
fn deinterleave(chunk: &[f32]) -> (Vec<f32>, Vec<f32>) {
    let n = chunk.len() / 2;
    let mut left = Vec::with_capacity(n);
    let mut right = Vec::with_capacity(n);
    for pair in chunk.chunks_exact(2) {
        left.push(pair[0]);
        right.push(pair[1]);
    }
    (left, right)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 440 Hz sine with a 5 ms fade-in/out to keep codec edges gentle.
    fn tone(frames: usize, rate: u32) -> Vec<f32> {
        let fade = (rate as usize / 200).max(1);
        (0..frames)
            .map(|i| {
                let t = i as f64 / rate as f64;
                let env = if i < fade {
                    i as f32 / fade as f32
                } else if i >= frames - fade {
                    (frames - i) as f32 / fade as f32
                } else {
                    1.0
                };
                ((2.0 * std::f64::consts::PI * 440.0 * t).sin() * 0.7) as f32 * env
            })
            .collect()
    }

    /// Align `b` against `a` by searching the lag with the lowest MSE in
    /// ±`window` samples, then return that lag (b starts `lag` samples
    /// later than a).
    fn best_lag(a: &[f32], b: &[f32], window: usize) -> usize {
        let mut best = (f64::MAX, 0usize);
        for lag in 0..=window {
            let mse: f64 = a
                .iter()
                .zip(b[lag..].iter())
                .take(20_000)
                .map(|(x, y)| {
                    let d = f64::from(x - y);
                    d * d
                })
                .sum::<f64>()
                / a.len().min(20_000) as f64;
            if mse < best.0 {
                best = (mse, lag);
            }
        }
        best.1
    }

    fn temp(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("mvl_mp3_test_{name}_{}.mp3", std::process::id()));
        p
    }

    #[test]
    fn target_rate_mapping() {
        assert_eq!(mp3_target_rate(192_000), 48_000);
        assert_eq!(mp3_target_rate(96_000), 48_000);
        assert_eq!(mp3_target_rate(48_000), 48_000);
        assert_eq!(mp3_target_rate(44_100), 44_100);
        assert_eq!(mp3_target_rate(22_050), 22_050);
        assert_eq!(mp3_target_rate(16_000), 16_000);
        assert_eq!(mp3_target_rate(8_000), 8_000);
        // Odd rates snap to the nearest legal rate.
        assert_eq!(mp3_target_rate(11_000), 11_025);
        assert_eq!(mp3_target_rate(45_000), 44_100);
    }

    #[test]
    fn encode_decode_roundtrip_mono_320kbps() {
        let path = temp("mono320");
        let rate: u32 = 48_000;
        let source = InterleavedAudio::new(tone(rate as usize, rate), rate, 1).unwrap();
        export(
            &path,
            &source,
            &Mp3Settings {
                bitrate: Mp3Bitrate::Kbps320,
                vbr: None,
            },
        )
        .unwrap();

        let back = import(&path).unwrap();
        assert_eq!(back.sample_rate, rate);
        assert_eq!(back.channels, 1);

        // MP3 adds encoder delay/padding: length may differ by a couple of
        // frames; content must line up after lag alignment with solid SNR.
        let max_dev = (f64::from(rate) * 0.10) as u64;
        assert!(
            (back.frames() as i64 - source.frames() as i64).unsigned_abs() < max_dev,
            "duration drift too large: {} vs {}",
            back.frames(),
            source.frames()
        );

        let lag = best_lag(&source.data, &back.data, 4_000);
        let compare = source.data.len() - lag - 1_000;
        let mut signal = 0.0f64;
        let mut noise = 0.0f64;
        for i in 0..compare {
            let s = f64::from(source.data[i]);
            let b = f64::from(back.data[lag + i]);
            signal += s * s;
            let d = s - b;
            noise += d * d;
        }
        let snr = 10.0 * (signal / noise.max(1e-12)).log10();
        assert!(snr > 25.0, "320 kbps sine SNR too low: {snr:.1} dB");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn encode_decode_roundtrip_stereo_192kbps() {
        let path = temp("stereo192");
        let rate: u32 = 44_100;
        let mono = tone(rate as usize, rate);
        let mut interleaved = Vec::with_capacity(mono.len() * 2);
        for m in &mono {
            interleaved.push(*m);
            interleaved.push(-*m); // inverted right channel keeps channels distinct
        }
        let source = InterleavedAudio::new(interleaved, rate, 2).unwrap();
        export(
            &path,
            &source,
            &Mp3Settings {
                bitrate: Mp3Bitrate::Kbps192,
                vbr: None,
            },
        )
        .unwrap();

        let back = import(&path).unwrap();
        assert_eq!(back.channels, 2);
        assert_eq!(back.sample_rate, rate);
        // Anti-phase channels must survive: the sum of channels stays near
        // zero and each channel keeps energy.
        let n = back.frames().min(40_000);
        let mut sum_energy = 0.0f64;
        let mut ch_energy = 0.0f64;
        for f in 0..n {
            let l = f64::from(back.data[f * 2]);
            let r = f64::from(back.data[f * 2 + 1]);
            sum_energy += (l + r) * (l + r);
            ch_energy += l * l;
        }
        assert!(ch_energy > 1.0, "channel content vanished");
        assert!(
            sum_energy / ch_energy < 0.1,
            "stereo image collapsed (sum/ch energy {})",
            sum_energy / ch_energy
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn vbr_export_produces_decodable_file() {
        let path = temp("vbr");
        let rate: u32 = 48_000;
        let source = InterleavedAudio::new(tone(rate as usize / 2, rate), rate, 1).unwrap();
        export(
            &path,
            &source,
            &Mp3Settings {
                bitrate: Mp3Bitrate::Kbps128,
                vbr: Some(VbrQuality::V2),
            },
        )
        .unwrap();
        let back = import(&path).unwrap();
        assert_eq!(back.sample_rate, 48_000);
        assert!(back.frames() > rate as usize / 4, "VBR file too short");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn high_rate_session_is_resampled_to_48k() {
        let path = temp("from192k");
        let source = InterleavedAudio::new(tone(96_000, 192_000u32), 192_000, 1).unwrap();
        export(
            &path,
            &source,
            &Mp3Settings {
                bitrate: Mp3Bitrate::Kbps256,
                vbr: None,
            },
        )
        .unwrap();
        let back = import(&path).unwrap();
        assert_eq!(
            back.sample_rate, 48_000,
            "192 kHz session must export at 48 kHz"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rejects_three_channels() {
        let source = InterleavedAudio::new(vec![0.0; 300], 48_000, 3).unwrap();
        let err = export(&temp("bad_ch"), &source, &Mp3Settings::default()).unwrap_err();
        assert!(matches!(err, Error::Mp3Encode(_)), "got {err:?}");
    }

    #[test]
    fn sniffs_mp3_magic() {
        let path = temp("sniff");
        let source = InterleavedAudio::new(tone(4_800, 48_000u32), 48_000, 1).unwrap();
        export(&path, &source, &Mp3Settings::default()).unwrap();
        assert!(looks_like_mp3(&path).unwrap());

        let mut wav_path = temp("notmp3");
        wav_path.set_extension("wav");
        let wav_src = InterleavedAudio::new(vec![0.1; 100], 48_000, 1).unwrap();
        crate::wav::export(&wav_path, &wav_src, crate::WavDepth::Float32).unwrap();
        assert!(!looks_like_mp3(&wav_path).unwrap());
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&wav_path);
    }
}
