//! Micro-Vocal Lab application binary.
//!
//! Phase 2: boot-time self check of the engine + I/O layer. The Slint UI
//! arrives in Phase 4; until then this binary verifies that every
//! subsystem links and initializes correctly — the same checks the UI
//! will run at startup.

fn main() {
    let params = mvl_core::VocalParams::neutral();
    println!(
        "Micro-Vocal Lab {} — phase 2 (audio I/O core)",
        env!("CARGO_PKG_VERSION")
    );
    println!(
        "engine: neutral params ok (pitch {} st, air {} %, tract {} mm)",
        params.pitch_semitones, params.air_percent, params.tract_mm
    );
    println!("audio host: {}", mvl_io::recorder::default_host_name());
    match mvl_io::recorder::default_input_device_name() {
        Ok(Some(name)) => println!("input device: {name}"),
        Ok(None) => println!("input device: none (recording unavailable on this machine)"),
        Err(e) => println!("input device: enumeration failed ({e})"),
    }
    println!(
        "codecs: WAV (hound) + MP3 decode (symphonia 0.5) + MP3 encode (LAME {})",
        mvl_io::mp3::lame_version()
    );
}
