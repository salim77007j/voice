//! # mvl-app — Micro-Vocal Lab application
//!
//! Phase 4: the Slint UI layer plus the glue that binds it to the
//! `mvl-core` DSP engine and the `mvl-io` audio stack. Everything the UI
//! shows is computed by the same engine code the export uses (plan §6.6);
//! there are no fake bindings.
//!
//! Module map:
//! * [`format`] — locale-neutral formatting/parsing helpers (timecodes,
//!   note names, numeric fields)
//! * [`waveform`] — peak mipmap, offscreen waveform rendering, ruler ticks,
//!   zoom math
//! * [`preview`] — real-time preview player (streaming engine → cpal)
//! * [`selftest`] — on-device verification kit (Phase 5): one command that
//!   exercises real hardware paths and collects evidence
//! * [`session`] — audio session model (import / record / export)
//! * [`controller`] — wires the Slint window to the session + player
//! * [`headless`] — offscreen platform for tests and screenshot rendering

slint::include_modules!();

pub mod controller;
pub mod format;
pub mod headless;
pub mod preview;
pub mod selftest;
pub mod session;
pub mod waveform;
