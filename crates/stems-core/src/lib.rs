//! Stem separation for dj: split whole tracks into drums, bass, vocals and other.
//!
//! - [`demucs`]: HT-Demucs (ONNX), the high-quality separator (features `onnx` / `onnx-dynamic`).
//! - [`offline`]: STFT mask separation with a small model ([`OnnxModel`], see
//!   `docs/stems/MODEL_CONTRACT.md`) or the [`HeuristicSeparator`] placeholder.
//!
//! Every function returns each stem as its own buffer, sample-aligned with the input.

#[cfg(any(feature = "onnx", feature = "onnx-dynamic"))]
pub mod demucs;
mod model;
pub mod offline;
#[cfg(any(feature = "onnx", feature = "onnx-dynamic"))]
mod onnx;
mod resample;
mod separator;

pub use model::{MaskModel, ModelError, ModelSpec, INPUT_PLANES};
#[cfg(any(feature = "onnx", feature = "onnx-dynamic"))]
pub use onnx::{OnnxModel, OnnxOptions};
pub use resample::resample;
pub use separator::{HeuristicSeparator, Separator, StemMasks};

pub use realfft::num_complex::Complex32;

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
