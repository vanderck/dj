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

/// Number of stems a track can be split into.
pub const NUM_STEMS: usize = 4;

/// Stem order used everywhere (gains, views, commands).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
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

    pub fn from_index(i: usize) -> Option<Stem> {
        Self::ALL.get(i).copied()
    }
}

/// Separated stems for one track, aligned sample-for-sample with its audio.
///
/// Only drums, bass and vocals are stored (16-bit, scaled by `scale`); "other" is the mix minus
/// those three, which is exact because the separator's masks sum to one. That keeps a 5-minute
/// track at ~160 MB instead of ~420 MB for four f32 stems.
pub struct StemData {
    /// Id of the [`Track`] these stems belong to.
    pub track_id: u64,
    /// Multiply a stored sample by this to get the linear value.
    pub scale: f32,
    pub drums: Vec<[i16; 2]>,
    pub bass: Vec<[i16; 2]>,
    pub vocals: Vec<[i16; 2]>,
}

impl StemData {
    /// Quantise float stems (drums, bass, vocals) to 16 bit.
    pub fn from_f32(track_id: u64, drums: &[Frame], bass: &[Frame], vocals: &[Frame]) -> Self {
        let peak = [drums, bass, vocals]
            .iter()
            .flat_map(|s| s.iter())
            .map(|f| f[0].abs().max(f[1].abs()))
            .fold(1.0f32, f32::max);
        let scale = peak / i16::MAX as f32;
        let q = |s: &[Frame]| -> Vec<[i16; 2]> {
            s.iter()
                .map(|f| [(f[0] / scale).round() as i16, (f[1] / scale).round() as i16])
                .collect()
        };
        Self {
            track_id,
            scale,
            drums: q(drums),
            bass: q(bass),
            vocals: q(vocals),
        }
    }

    pub fn len(&self) -> usize {
        self.drums.len()
    }

    pub fn is_empty(&self) -> bool {
        self.drums.is_empty()
    }

    /// Mix of the stems for frame `i` of a track whose mixed frame is `mix`, with per-stem
    /// linear `gains` in [`Stem`] order.
    #[inline]
    pub fn mix(&self, i: usize, mix: Frame, gains: &[f32; NUM_STEMS]) -> Frame {
        let get = |v: &Vec<[i16; 2]>| v.get(i).copied().unwrap_or([0, 0]);
        let (d, b, v) = (get(&self.drums), get(&self.bass), get(&self.vocals));
        let g_other = gains[Stem::Other as usize];
        let kd = (gains[Stem::Drums as usize] - g_other) * self.scale;
        let kb = (gains[Stem::Bass as usize] - g_other) * self.scale;
        let kv = (gains[Stem::Vocals as usize] - g_other) * self.scale;
        let mut out = [0.0; 2];
        for c in 0..2 {
            out[c] = g_other * mix[c] + kd * d[c] as f32 + kb * b[c] as f32 + kv * v[c] as f32;
        }
        out
    }

    /// Bytes of audio held (for cache sizing / UI).
    pub fn bytes(&self) -> usize {
        3 * self.len() * std::mem::size_of::<[i16; 2]>()
    }
}

#[cfg(test)]
mod stem_tests {
    use super::*;

    fn stems() -> (Vec<Frame>, [Vec<Frame>; 4]) {
        let n = 1000;
        let gen = |f: f32, a: f32| -> Vec<Frame> {
            (0..n)
                .map(|i| {
                    let v = a * (i as f32 * f).sin();
                    [v, 0.5 * v]
                })
                .collect()
        };
        let s = [
            gen(0.01, 0.3),
            gen(0.002, 0.5),
            gen(0.05, 0.2),
            gen(0.13, 0.25),
        ];
        let mix = (0..n)
            .map(|i| {
                let mut f = [0.0; 2];
                for st in &s {
                    f[0] += st[i][0];
                    f[1] += st[i][1];
                }
                f
            })
            .collect();
        (mix, s)
    }

    #[test]
    fn unity_gains_return_the_mix_and_solo_isolates() {
        let (mix, s) = stems();
        let data = StemData::from_f32(7, &s[0], &s[1], &s[2]);
        for i in [0, 10, 500, 999] {
            let all = data.mix(i, mix[i], &[1.0; 4]);
            assert!((all[0] - mix[i][0]).abs() < 1e-6);
            for (k, stem) in s.iter().enumerate() {
                let mut g = [0.0; 4];
                g[k] = 1.0;
                let solo = data.mix(i, mix[i], &g);
                // 16-bit quantisation of three stems => error well below -80 dBFS.
                assert!((solo[0] - stem[i][0]).abs() < 1e-4, "stem {k} frame {i}");
                assert!((solo[1] - stem[i][1]).abs() < 1e-4);
            }
        }
    }

    #[test]
    fn handles_loud_stems_and_short_buffers() {
        let loud = vec![[1.8f32, -1.8]; 4];
        let data = StemData::from_f32(1, &loud, &loud, &loud);
        assert!((data.mix(0, [0.0; 2], &[1.0, 0.0, 0.0, 0.0])[0] - 1.8).abs() < 1e-3);
        // Reading past the end yields the plain mix scaled by the "other" gain.
        assert_eq!(data.mix(100, [0.5, 0.5], &[0.0, 0.0, 0.0, 1.0]), [0.5, 0.5]);
    }
}
