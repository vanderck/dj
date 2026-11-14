//! In-memory track representation shared between the loader and the engine.

use crate::dsp::Frame;
use serde::{Deserialize, Serialize};

/// Decoded audio, stereo, at the file's native sample rate.
pub struct AudioData {
    pub sample_rate: u32,
    pub frames: Vec<Frame>,
}

impl AudioData {
    pub fn duration_secs(&self) -> f64 {
        self.frames.len() as f64 / self.sample_rate as f64
    }
}

/// Beat grid: constant tempo anchored at a "Grid Start" (the first downbeat).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct BeatGrid {
    pub bpm: f64,
    /// Position of a downbeat (bar 1, beat 1) in seconds.
    pub anchor_secs: f64,
}

impl BeatGrid {
    pub fn seconds_per_beat(&self) -> f64 {
        60.0 / self.bpm
    }

    /// Beat number (fractional) at a time in seconds; beat 0 is Grid Start.
    pub fn beat_at(&self, secs: f64) -> f64 {
        (secs - self.anchor_secs) / self.seconds_per_beat()
    }

    pub fn secs_at_beat(&self, beat: f64) -> f64 {
        self.anchor_secs + beat * self.seconds_per_beat()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum CueKind {
    #[default]
    Hot,
    /// Loop cue: captures a loop exactly.
    Loop,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Cue {
    pub kind: CueKind,
    pub secs: f64,
    /// Loop end for loop cues.
    pub end_secs: Option<f64>,
    pub name: String,
    /// RGB colour (from the tag if present).
    pub color: [u8; 3],
}

pub const NUM_HOT_CUES: usize = 8;

/// Everything the engine needs to play a track. Shared via `Arc`.
pub struct Track {
    /// Unique id assigned by the loader so the UI can match snapshots.
    pub id: u64,
    pub audio: AudioData,
    pub grid: Option<BeatGrid>,
    /// Gain applied before trim (e.g. from ReplayGain tags), linear.
    pub gain: f32,
    pub cues: [Option<Cue>; NUM_HOT_CUES],
}
