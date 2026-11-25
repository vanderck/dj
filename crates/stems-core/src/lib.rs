//! Stem separation for dj: split whole tracks into drums, bass, vocals and other with HT-Demucs
//! ([`demucs`], ONNX, features `onnx` / `onnx-dynamic`).
//!
//! Every stem comes back as its own buffer, sample-aligned with the input.

#[cfg(any(feature = "onnx", feature = "onnx-dynamic"))]
pub mod demucs;
pub mod offline;
#[cfg(any(feature = "onnx", feature = "onnx-dynamic"))]
mod onnx;
mod resample;
pub mod structure;

pub use offline::ModelError;
#[cfg(any(feature = "onnx", feature = "onnx-dynamic"))]
pub use onnx::{Device, OnnxOptions};
pub use resample::resample;

/// Number of stems.
pub const NUM_STEMS: usize = 4;

/// The stems, in the order used by every per-stem array in this crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stem {
    Drums = 0,
    Bass = 1,
    Vocals = 2,
    Other = 3,
}

impl Stem {
    pub const ALL: [Stem; NUM_STEMS] = [Stem::Drums, Stem::Bass, Stem::Vocals, Stem::Other];

    pub fn name(self) -> &'static str {
        match self {
            Stem::Drums => "Drums",
            Stem::Bass => "Bass",
            Stem::Vocals => "Vocals",
            Stem::Other => "Other",
        }
    }
}
