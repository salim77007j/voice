//! # mvl-core — Micro-Vocal Lab DSP engine
//!
//! Pure DSP library for microscopic voice control. This crate has **no
//! audio-hardware or file-system dependencies**: everything is plain
//! `f32` signal in / signal out so the whole engine is unit-testable
//! headless in CI.
//!
//! Phase 3 scope (plan §6): the processing chain — shared STFT analysis,
//! frame classification, pitch (phase-locked phase vocoder), formant
//! (true-envelope warp) and air/breath engine — assembled by
//! [`VocalEngine`] with the preview/render quality profiles of plan §6.6.

pub mod analysis;
pub mod breath;
pub mod engine;
pub mod error;
pub mod formant;
pub mod limiter;
pub mod params;
pub mod pitch;
pub mod spectrum;
pub mod stft;
pub mod testsupport;

pub use analysis::{ClassSmoother, FrameAnalyzer, FrameClass, FrameFeatures};
pub use breath::{BreathEdit, BreathProcessor};
pub use engine::{RenderResult, VocalEngine};
pub use error::EngineError;
pub use formant::FormantProcessor;
pub use limiter::{guard_offline, sample_peak, true_peak};
pub use params::{QualityProfile, VocalParams};
pub use pitch::{PhaseVocoder, PitchPath, RatioConverter};
pub use spectrum::{
    db_to_meter, RtaAnalyzer, RtaSnapshot, CLIP_DBFS, METER_FLOOR_DB, RTA_BANDS, RTA_FLOOR_DB,
};
pub use stft::{AnalysisFrame, OverlapAdder, StftAnalyzer};
