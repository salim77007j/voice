//! # mvl-core — Micro-Vocal Lab DSP engine
//!
//! Pure DSP library for microscopic voice control. This crate has **no
//! audio-hardware or file-system dependencies**: everything is plain
//! `f32` signal in / signal out so the whole engine is unit-testable
//! headless in CI.
//!
//! Phase 3 scope (plan §6): the processing chain — shared STFT analysis,
//! frame classification, pitch (phase-locked phase vocoder), formant
//! (true-envelope warp) and air/breath engine, assembled by the
//! VocalEngine (landing later in Phase 3).

pub mod analysis;
pub mod params;
pub mod stft;
pub mod testsupport;

pub use analysis::{ClassSmoother, FrameAnalyzer, FrameClass, FrameFeatures};
pub use params::{QualityProfile, VocalParams};
pub use stft::{AnalysisFrame, OverlapAdder, StftAnalyzer};
