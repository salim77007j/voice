//! Dev probe: what does pitch-detection 0.3 YIN actually return?
use mvl_core::testsupport as ts;
use pitch_detection::detector::yin::YINDetector;
use pitch_detection::detector::PitchDetector;

fn probe(name: &str, sig: &[f32], rate: usize, size: usize) {
    let sig64: Vec<f64> = sig.iter().map(|s| f64::from(*s)).collect();
    let slice = &sig64[sig64.len() - size..];
    let mut det = YINDetector::<f64>::new(size, 0);
    let got = det.get_pitch(slice, rate, 1e-4, 0.85);
    let (f0, clarity) = match &got {
        Some(p) => (p.frequency, p.clarity),
        None => (f64::NAN, f64::NAN),
    };
    // Own measure: normalised autocorrelation at the detected period.
    let ac = if let Some(p) = &got {
        let lag = (rate as f64 / p.frequency).round() as usize;
        let n = slice.len() - lag;
        let mut num = 0.0;
        let mut den = 0.0;
        for i in 0..n {
            num += slice[i] * slice[i + lag];
            den += slice[i] * slice[i];
        }
        (num / den.max(1e-30)).clamp(0.0, 1.0)
    } else {
        f64::NAN
    };
    println!("{name:12} f0={f0:9.3} crate_clarity={clarity:8.4} autocorr={ac:.4}");
}

fn main() {
    let rate = 48_000usize;
    let len = 48_000;
    probe(
        "stack196x16",
        &ts::harmonic_stack(196.0, 16, 0.5, len, rate as u32),
        rate,
        1024,
    );
    probe(
        "sine220",
        &ts::sine(220.0, 0.5, len, rate as u32),
        rate,
        1024,
    );
    probe(
        "stack85",
        &ts::harmonic_stack(85.0, 24, 0.5, len, rate as u32),
        rate,
        1024,
    );
    probe(
        "stack85-2048",
        &ts::harmonic_stack(85.0, 24, 0.5, len, rate as u32),
        rate,
        2048,
    );
    probe(
        "vowel196",
        &ts::formant_vowel(
            196.0,
            &[(730.0, 10.0), (1090.0, 10.0)],
            0.5,
            len,
            rate as u32,
        ),
        rate,
        1024,
    );
    probe("noise", &ts::white_noise(3, len), rate, 1024);
    probe(
        "breath",
        &ts::breath_noise(11, len, rate as u32, 0.063),
        rate,
        1024,
    );
    probe(
        "sibilant",
        &ts::sibilant_noise(7, len, rate as u32, 0.1),
        rate,
        1024,
    );
    probe("silence", &ts::silence(len), rate, 1024);
}
