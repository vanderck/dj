use crate::{Complex32, NUM_STEMS};

/// Soft masks, one per stem, each `num_bins` long.
///
/// Contract for [`Separator`] implementations: every value is in `0..=1` and, per bin, the four
/// masks sum to 1. That guarantees that with all stems at unity gain and isolation 0 the engine
/// reconstructs the input exactly (apart from the latency).
pub struct StemMasks {
    pub masks: [Vec<f32>; NUM_STEMS],
}

impl StemMasks {
    pub fn new(num_bins: usize) -> Self {
        Self {
            masks: std::array::from_fn(|_| vec![0.0; num_bins]),
        }
    }

    pub fn num_bins(&self) -> usize {
        self.masks[0].len()
    }
}

/// The source-separation model.
///
/// The engine calls [`Separator::separate`] once per STFT hop, on the audio thread, with the
/// newest stereo spectrum. Implementations must not allocate, lock or block in `separate`.
///
/// Models that need future context return a non-zero [`Separator::lookahead_frames`]: the masks
/// written by `separate` then belong to the frame `lookahead_frames` hops *before* the one passed
/// in, and the engine delays the spectrum accordingly (adding `lookahead_frames * hop` samples of
/// latency, which the plugin reports to the host).
///
/// A neural model (ONNX via `ort`, Core ML, ...) whose inference is too slow or too bursty for
/// the audio thread should run on a worker thread and exchange frames through lock-free ring
/// buffers; its `separate` then just pushes the new frame and pops the masks that are due. See
/// `docs/ARCHITECTURE.md`.
pub trait Separator: Send {
    fn lookahead_frames(&self) -> usize {
        0
    }

    /// Clear internal state (called when playback restarts).
    fn reset(&mut self) {}

    /// Offline (non-real-time) rendering: the separator may block until its results are ready
    /// instead of falling back to stale ones, so bounces are always fully separated.
    fn set_offline(&mut self, _offline: bool) {}

    /// `left`/`right` are the `fft_size / 2 + 1` complex bins of the newest frame.
    fn separate(&mut self, left: &[Complex32], right: &[Complex32], masks: &mut StemMasks);
}

/// Placeholder separator built from simple signal heuristics, **not** a trained model:
///
/// - Bass: energy below ~150 Hz.
/// - Drums: transient energy (onsets relative to a running per-bin average).
/// - Vocals: phantom-centre (L/R-coherent) energy in the 200 Hz – 6 kHz band.
/// - Other: everything else.
///
/// It exists so the full signal path, UI and host integration can be built and tested before a
/// real model is available. Expect heavy bleed between stems.
pub struct HeuristicSeparator {
    bass_prior: Vec<f32>,
    vocal_band: Vec<f32>,
    avg_mag: Vec<f32>,
}

impl HeuristicSeparator {
    pub fn new(sample_rate: f32, fft_size: usize) -> Self {
        let num_bins = fft_size / 2 + 1;
        let bin_hz = sample_rate / fft_size as f32;
        let freqs: Vec<f32> = (0..num_bins).map(|k| k as f32 * bin_hz).collect();
        let ramp = |x: f32, a: f32, b: f32| ((x - a) / (b - a)).clamp(0.0, 1.0);

        Self {
            bass_prior: freqs.iter().map(|&f| 1.0 - ramp(f, 120.0, 250.0)).collect(),
            vocal_band: freqs
                .iter()
                .map(|&f| ramp(f, 150.0, 250.0) * (1.0 - ramp(f, 5_000.0, 8_000.0)))
                .collect(),
            avg_mag: vec![0.0; num_bins],
        }
    }
}

impl Separator for HeuristicSeparator {
    fn reset(&mut self) {
        self.avg_mag.fill(0.0);
    }

    fn separate(&mut self, left: &[Complex32], right: &[Complex32], out: &mut StemMasks) {
        let [drums, bass, vocals, other] = &mut out.masks;

        for k in 0..left.len() {
            let (l, r) = (left[k], right[k]);
            let power = 0.5 * (l.norm_sqr() + r.norm_sqr());
            let mag = power.sqrt();

            // Transient score: how far this frame jumps above the running average.
            let avg = self.avg_mag[k];
            let perc = ((mag - 1.5 * avg).max(0.0) / (mag + 1e-9)).min(1.0);
            self.avg_mag[k] = 0.8 * avg + 0.2 * mag;

            // 1 when L and R are identical (phantom centre), 0 for uncorrelated / out of phase.
            let coherence = ((l * r.conj()).re / (power + 1e-12)).clamp(0.0, 1.0);

            let bp = self.bass_prior[k];
            let w_bass = bp * (1.0 - 0.7 * perc);
            let w_drums = perc * (1.0 - 0.5 * bp) + 0.05;
            let w_vocals = self.vocal_band[k] * coherence * coherence * (1.0 - perc) * (1.0 - bp);
            let w_other = 0.25;

            let norm = 1.0 / (w_bass + w_drums + w_vocals + w_other);
            drums[k] = w_drums * norm;
            bass[k] = w_bass * norm;
            vocals[k] = w_vocals * norm;
            other[k] = w_other * norm;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heuristic_masks_are_a_partition_of_unity() {
        let n = 1024;
        let mut sep = HeuristicSeparator::new(44_100.0, n);
        let mut masks = StemMasks::new(n / 2 + 1);
        let mut seed = 1u32;
        let mut rnd = || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5
        };
        for _ in 0..8 {
            let l: Vec<_> = (0..n / 2 + 1)
                .map(|_| Complex32::new(rnd(), rnd()))
                .collect();
            let r: Vec<_> = (0..n / 2 + 1)
                .map(|_| Complex32::new(rnd(), rnd()))
                .collect();
            sep.separate(&l, &r, &mut masks);
            for k in 0..masks.num_bins() {
                let sum: f32 = masks.masks.iter().map(|m| m[k]).sum();
                assert!((sum - 1.0).abs() < 1e-5, "bin {k}: {sum}");
                assert!(masks.masks.iter().all(|m| (0.0..=1.0).contains(&m[k])));
            }
        }
    }
}
