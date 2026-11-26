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

/// The tempos a track's beat grid can plausibly have; a grid or tempo map outside them is a bad
/// tag or a bad measurement (a Serato marker with the wrong beat count gave a 0.4 BPM segment).
pub const PLAUSIBLE_BPM: std::ops::RangeInclusive<f64> = 30.0..=300.0;

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

/// A tempo that changes through a track (a live drummer, a speed-up, several Serato grid
/// markers): where given beats are, in seconds, with the tempo steady between them and the end
/// segments carried on beyond. Beat numbers are the grid's (beat 0 is a downbeat).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TempoMap {
    /// (beat, seconds), both increasing; at least two.
    pub points: Vec<(f64, f64)>,
}

impl TempoMap {
    /// A map from (beat, seconds) points (sorted, duplicates dropped); `None` with fewer than two,
    /// or when it isn't a believable tempo (see [`TempoMap::plausible`]).
    pub fn new(mut points: Vec<(f64, f64)>) -> Option<Self> {
        points.sort_by(|a, b| a.0.total_cmp(&b.0));
        points.dedup_by(|b, a| b.0 <= a.0 || b.1 <= a.1);
        let m = TempoMap { points };
        (m.points.len() >= 2 && m.plausible()).then_some(m)
    }

    /// Every segment's tempo within [`PLAUSIBLE_BPM`] and within half to double of the median
    /// segment's: a tempo that bends, not a marker with the wrong beat count.
    pub fn plausible(&self) -> bool {
        let mut bpms: Vec<f64> = self.points.windows(2).map(|w| 60.0 * (w[1].0 - w[0].0) / (w[1].1 - w[0].1)).collect();
        if bpms.is_empty() || bpms.iter().any(|b| !b.is_finite() || !PLAUSIBLE_BPM.contains(b)) {
            return false;
        }
        bpms.sort_by(f64::total_cmp);
        let med = bpms[bpms.len() / 2];
        bpms.iter().all(|b| (0.5..=2.0).contains(&(b / med)))
    }

    /// The segment index (into `points`, its start) covering `x` on axis `axis` (0 beats, 1 secs).
    fn seg(&self, x: f64, axis: usize) -> usize {
        let p = &self.points;
        let i = p.partition_point(|q| if axis == 0 { q.0 } else { q.1 } <= x);
        i.saturating_sub(1).min(p.len() - 2)
    }

    pub fn secs_at_beat(&self, beat: f64) -> f64 {
        let i = self.seg(beat, 0);
        let ((b0, s0), (b1, s1)) = (self.points[i], self.points[i + 1]);
        s0 + (beat - b0) * (s1 - s0) / (b1 - b0)
    }

    pub fn beat_at(&self, secs: f64) -> f64 {
        let i = self.seg(secs, 1);
        let ((b0, s0), (b1, s1)) = (self.points[i], self.points[i + 1]);
        b0 + (secs - s0) * (b1 - b0) / (s1 - s0)
    }

    /// The steady grid that matches the map at `secs` (its tempo there, its beats there).
    pub fn local(&self, secs: f64) -> BeatGrid {
        let i = self.seg(secs, 1);
        let ((b0, s0), (b1, s1)) = (self.points[i], self.points[i + 1]);
        let spb = (s1 - s0) / (b1 - b0);
        BeatGrid { bpm: 60.0 / spb, anchor_secs: s0 - b0 * spb }
    }

    /// The overall steady grid (first to last point).
    pub fn average(&self) -> BeatGrid {
        let ((b0, s0), (b1, s1)) = (self.points[0], self.points[self.points.len() - 1]);
        let spb = (s1 - s0) / (b1 - b0);
        BeatGrid { bpm: 60.0 / spb, anchor_secs: s0 - b0 * spb }
    }

    /// Every time moved by `secs`.
    pub fn shift_secs(&mut self, secs: f64) {
        self.points.iter_mut().for_each(|p| p.1 += secs);
    }

    /// Every beat number moved by `beats` (a new downbeat).
    pub fn shift_beats(&mut self, beats: f64) {
        self.points.iter_mut().for_each(|p| p.0 += beats);
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
    /// When the tempo changes through the track: the grid follows this map (see [`TempoMap`]).
    pub tempo_map: Option<TempoMap>,
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
            s.iter().map(|f| [(f[0] / scale).round() as i16, (f[1] / scale).round() as i16]).collect()
        };
        Self { track_id, scale, drums: q(drums), bass: q(bass), vocals: q(vocals) }
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
            (0..n).map(|i| {
                let v = a * (i as f32 * f).sin();
                [v, 0.5 * v]
            }).collect()
        };
        let s = [gen(0.01, 0.3), gen(0.002, 0.5), gen(0.05, 0.2), gen(0.13, 0.25)];
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

#[cfg(test)]
mod tempo_map_tests {
    #[test]
    fn a_marker_with_the_wrong_beat_count_is_no_tempo_map() {
        // One beat over two and a half minutes (0.4 BPM), then 128 BPM.
        let spb = 60.0 / 128.0;
        assert!(TempoMap::new(vec![(0.0, 0.1), (1.0, 146.0), (65.0, 146.0 + 64.0 * spb)]).is_none());
        // A real bend, 120 to 132: kept.
        assert!(TempoMap::new(vec![(0.0, 0.0), (64.0, 32.0), (128.0, 32.0 + 64.0 * 60.0 / 132.0)]).is_some());
    }

    use super::*;

    #[test]
    fn a_map_bends_the_grid() {
        // 120 BPM for 64 beats, then 124 BPM.
        let (t64, spb2) = (64.0 * 0.5, 60.0 / 124.0);
        let m = TempoMap::new(vec![(0.0, 0.0), (64.0, t64), (128.0, t64 + 64.0 * spb2)]).unwrap();
        assert!((m.secs_at_beat(32.0) - 16.0).abs() < 1e-9);
        assert!((m.secs_at_beat(96.0) - (t64 + 32.0 * spb2)).abs() < 1e-9);
        assert!((m.beat_at(t64 + 10.0 * spb2) - 74.0).abs() < 1e-9);
        // Beyond the last point: its tempo carries on.
        assert!((m.secs_at_beat(200.0) - (t64 + 136.0 * spb2)).abs() < 1e-9);
        // The local grid: the tempo there, matching the map's beat there.
        let g = m.local(t64 + 5.0);
        assert!((g.bpm - 124.0).abs() < 1e-9);
        assert!((g.beat_at(t64 + 5.0) - m.beat_at(t64 + 5.0)).abs() < 1e-9);
        assert!((m.local(3.0).bpm - 120.0).abs() < 1e-9);
    }
}
