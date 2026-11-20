//! Types shared by whole-track separation: stereo frames, the stems, progress and errors.

use crate::NUM_STEMS;

/// Stereo frame, `[left, right]`.
pub type Frame = [f32; 2];

/// Separated stems in [`crate::Stem`] order, each the same length and sample rate as the input.
pub type StemBuffers = [Vec<Frame>; NUM_STEMS];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OfflineError {
    /// The progress callback returned `false`.
    Cancelled,
    /// The model failed.
    Model(String),
}

impl std::fmt::Display for OfflineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OfflineError::Cancelled => f.write_str("cancelled"),
            OfflineError::Model(e) => write!(f, "model error: {e}"),
        }
    }
}

impl std::error::Error for OfflineError {}

/// Progress callback: receives the fraction done (0..=1); return `false` to cancel.
pub type Progress<'a> = &'a mut dyn FnMut(f32) -> bool;

/// A model file that can't be loaded or run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelError(pub String);

impl ModelError {
    pub fn new(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }
}

impl std::fmt::Display for ModelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ModelError {}
