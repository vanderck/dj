//! The interface between offline separation and a mask-estimating model.
//!
//! See `docs/MODEL_CONTRACT.md` for the exact tensor layout and STFT conventions a trained model
//! must follow.

use std::fmt;

use crate::NUM_STEMS;

/// Number of input feature planes per frame: `L.re, L.im, R.re, R.im`.
pub const INPUT_PLANES: usize = 4;

/// Static description of a mask-estimating model.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelSpec {
    /// Sample rate the model was trained at.
    pub sample_rate: f32,
    /// STFT size; the hop is always `fft_size / 4`.
    pub fft_size: usize,
    /// Frames per inference window (`T`), oldest first.
    pub context_frames: usize,
    /// How many frames of the window lie *after* the frame the masks are for.
    pub lookahead_frames: usize,
}

impl ModelSpec {
    pub fn num_bins(&self) -> usize {
        self.fft_size / 2 + 1
    }

    pub fn hop_size(&self) -> usize {
        self.fft_size / 4
    }

    /// Floats in one input frame (`[INPUT_PLANES][num_bins]`).
    pub fn frame_len(&self) -> usize {
        INPUT_PLANES * self.num_bins()
    }

    /// Floats in the whole input window (`[context_frames][INPUT_PLANES][num_bins]`).
    pub fn input_len(&self) -> usize {
        self.context_frames * self.frame_len()
    }

    /// Floats in the output (`[NUM_STEMS][num_bins]`).
    pub fn output_len(&self) -> usize {
        NUM_STEMS * self.num_bins()
    }

    pub fn validate(&self) -> Result<(), ModelError> {
        if !self.fft_size.is_power_of_two() || self.fft_size < 64 {
            return Err(ModelError::new(format!(
                "fft_size must be a power of two >= 64, got {}",
                self.fft_size
            )));
        }
        if self.context_frames == 0 || self.lookahead_frames >= self.context_frames {
            return Err(ModelError::new(format!(
                "need lookahead_frames < context_frames, got {} / {}",
                self.lookahead_frames, self.context_frames
            )));
        }
        if !(8_000.0..=384_000.0).contains(&self.sample_rate) {
            return Err(ModelError::new(format!(
                "implausible sample_rate {}",
                self.sample_rate
            )));
        }
        Ok(())
    }
}

/// A mask-estimating model.
pub trait MaskModel {
    fn spec(&self) -> ModelSpec;

    /// Human-readable name for the UI.
    fn name(&self) -> &str {
        "model"
    }

    /// `context` holds `spec().input_len()` floats laid out `[T][INPUT_PLANES][F]`, oldest frame
    /// first. Write `spec().output_len()` floats laid out `[NUM_STEMS][F]` into `masks`: the
    /// masks for frame `T - 1 - lookahead_frames`, stems in [`crate::Stem`] order.
    ///
    /// Outputs are sanitised afterwards (clamped to `0..=1`, normalised to sum to 1 per bin), so a
    /// model may return unnormalised non-negative weights.
    fn infer(&mut self, context: &[f32], masks: &mut [f32]) -> Result<(), ModelError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelError(pub String);

impl ModelError {
    pub fn new(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }
}

impl fmt::Display for ModelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ModelError {}

/// Clamp to `0..=1` (NaN → 0) and normalise each bin so the stems sum to 1. Bins with no weight
/// at all become evenly split.
pub(crate) fn sanitize_masks(masks: &mut [f32], num_bins: usize) {
    for k in 0..num_bins {
        let mut sum = 0.0;
        for s in 0..NUM_STEMS {
            let m = &mut masks[s * num_bins + k];
            *m = if m.is_finite() {
                m.clamp(0.0, 1.0)
            } else {
                0.0
            };
            sum += *m;
        }
        for s in 0..NUM_STEMS {
            let m = &mut masks[s * num_bins + k];
            *m = if sum > 1e-9 {
                *m / sum
            } else {
                1.0 / NUM_STEMS as f32
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_handles_garbage() {
        let f = 3;
        let mut m = vec![
            f32::NAN,
            2.0,
            0.0, // drums
            -1.0,
            2.0,
            0.0, // bass
            1.0,
            0.0,
            0.0, // vocals
            1.0,
            0.0,
            0.0, // other
        ];
        sanitize_masks(&mut m, f);
        for k in 0..f {
            let sum: f32 = (0..NUM_STEMS).map(|s| m[s * f + k]).sum();
            assert!((sum - 1.0).abs() < 1e-6);
        }
        assert_eq!(m[0], 0.0);
        assert_eq!(m[6], 0.5);
        assert_eq!(m[2], 0.25); // all-zero bin becomes even
    }

    #[test]
    fn validate_rejects_bad_specs() {
        let ok = ModelSpec {
            sample_rate: 44_100.0,
            fft_size: 4096,
            context_frames: 8,
            lookahead_frames: 2,
        };
        assert!(ok.validate().is_ok());
        assert!(ModelSpec {
            fft_size: 1000,
            ..ok
        }
        .validate()
        .is_err());
        assert!(ModelSpec {
            lookahead_frames: 8,
            ..ok
        }
        .validate()
        .is_err());
    }
}
