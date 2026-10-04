//! Micro-Vocal Lab application binary.
//!
//! Phase 2: boot-time self check of the engine core. The Slint UI arrives
//! in Phase 4; until then this binary verifies that the workspace builds
//! and the parameter model is sane.

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
}
