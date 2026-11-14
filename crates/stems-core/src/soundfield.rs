use crate::NUM_STEMS;

/// Number of log-spaced frequency bands in a [`SoundfieldFrame`].
pub const SOUNDFIELD_BANDS: usize = 64;
/// Lowest / highest band centre frequency shown.
pub const SOUNDFIELD_MIN_HZ: f32 = 30.0;
pub const SOUNDFIELD_MAX_HZ: f32 = 16_000.0;

/// One stem's energy in one frequency band.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SoundfieldPoint {
    /// Energy-weighted stereo position, -1 (left) ..= 1 (right).
    pub pan: f32,
    /// Level in dBFS-ish units (relative to a full-scale sine in a single bin).
    pub level_db: f32,
}

/// A snapshot of where each stem sits in frequency and stereo position. Plain-old-data (`Copy`)
/// so it can be handed to the GUI through a lock-free triple buffer without allocating.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SoundfieldFrame {
    /// `points[stem][band]`; band 0 is the lowest frequency.
    pub points: [[SoundfieldPoint; SOUNDFIELD_BANDS]; NUM_STEMS],
}

impl Default for SoundfieldFrame {
    fn default() -> Self {
        Self {
            points: [[SoundfieldPoint {
                pan: 0.0,
                level_db: -120.0,
            }; SOUNDFIELD_BANDS]; NUM_STEMS],
        }
    }
}

/// Centre frequency of a soundfield band.
pub fn band_center_hz(band: usize) -> f32 {
    let t = band as f32 / (SOUNDFIELD_BANDS - 1) as f32;
    SOUNDFIELD_MIN_HZ * (SOUNDFIELD_MAX_HZ / SOUNDFIELD_MIN_HZ).powf(t)
}

/// Band index for a frequency, or `None` if it is outside the displayed range.
pub(crate) fn band_for_hz(freq_hz: f32) -> Option<usize> {
    if !(SOUNDFIELD_MIN_HZ..=SOUNDFIELD_MAX_HZ).contains(&freq_hz) {
        return None;
    }
    let t = (freq_hz / SOUNDFIELD_MIN_HZ).ln() / (SOUNDFIELD_MAX_HZ / SOUNDFIELD_MIN_HZ).ln();
    Some(((t * (SOUNDFIELD_BANDS - 1) as f32).round() as usize).min(SOUNDFIELD_BANDS - 1))
}

/// Per-band accumulator used while analysing one STFT frame.
#[derive(Clone, Copy, Default)]
pub(crate) struct BandAccum {
    pub energy: f32,
    pub pan_energy: f32,
}
