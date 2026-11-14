//! Real-time stem separation engine.
//!
//! This crate contains everything that is *not* plugin glue: the streaming STFT, the
//! [`Separator`] abstraction (where an ML model plugs in), stem mixing with isolation, the
//! Focus EQ (frequency + stereo-position isolation) and the soundfield analysis used by the UI.
//!
//! It has no dependency on any plugin framework so it can be unit-tested, benchmarked and reused
//! from other front-ends (an offline CLI, a Swift/AUv3 shell over a C ABI, ...).
//!
//! Real-time contract: after [`StemEngine::new`] returns, [`StemEngine::process`] and
//! [`StemEngine::reset`] never allocate, lock or block.

mod engine;
mod focus;
mod separator;
mod settings;
mod soundfield;

pub use engine::StemEngine;
pub use focus::focus_weight;
pub use separator::{HeuristicSeparator, Separator, StemMasks};
pub use settings::{FocusMode, FocusSettings, Settings, StemSettings};
pub use soundfield::{
    band_center_hz, SoundfieldFrame, SoundfieldPoint, SOUNDFIELD_BANDS, SOUNDFIELD_MAX_HZ,
    SOUNDFIELD_MIN_HZ,
};

pub use realfft::num_complex::Complex32;

/// Number of stems produced by the separator.
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
