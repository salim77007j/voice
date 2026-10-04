//! # mvl-core — Micro-Vocal Lab DSP engine
//!
//! Pure DSP library for microscopic voice control. This crate has **no
//! audio-hardware or file-system dependencies**: everything is plain
//! `f32` signal in / signal out so the whole engine is unit-testable
//! headless in CI.
//!
//! Phase 2 scope: parameter model shared by the I/O layer, transport and
//! the (Phase 3) processing chain. The processing modules land in Phase 3
//! per `docs/ARCHITECTURE_PLAN.md` §6.

pub mod params;

pub use params::{QualityProfile, VocalParams};
