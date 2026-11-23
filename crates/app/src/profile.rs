//! A track's mix profile: what the transition search needs to score a mix with it (its bars,
//! phrases, energy and sections), kept on disk next to the prepared stems so tracks that aren't
//! loaded can be weighed (play-next suggestions, Auto-DJ). Made when a track has been on a deck,
//! and for the tracks you prepare in the library (see [`prepare_track`]).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use dj_engine::track::{BeatGrid, Cue};

use crate::stems::{self, Section, StemWave};
use crate::transition::{phrase_offset, Bars};

const VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Profile {
    version: u32,
    /// The file's size and modification time when it was made (a changed file is made again).
    stamp: (u64, u64),
    /// The grid's tempo.
    pub bpm: f64,
    bars: Bars,
    /// Its cue points, in bars.
    pub cue_bars: Vec<f64>,
    /// Its sections (start, end, label).
    sections: Vec<(f64, f64, String)>,
    /// Made with stems (else from the waveform's bands alone).
    pub stems: bool,
}

/// The file's size and modification time.
fn stamp(path: &Path) -> (u64, u64) {
    let meta = std::fs::metadata(path).ok();
    let mtime = meta.as_ref().and_then(|m| m.modified().ok()).and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
    (meta.map_or(0, |m| m.len()), mtime)
}

/// Where a track's profile is kept.
pub fn file(dir: &Path, path: &Path) -> PathBuf {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in path.to_string_lossy().as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    dir.join("profiles").join(format!("{h:016x}.json"))
}

/// A track's profile, if it was made (for this version of the file).
pub fn read(dir: &Path, path: &Path) -> Option<Profile> {
    let p: Profile = serde_json::from_slice(&std::fs::read(file(dir, path)).ok()?).ok()?;
    (p.version == VERSION && p.stamp == stamp(path)).then_some(p)
}

pub fn write(dir: &Path, path: &Path, p: &Profile) -> std::io::Result<()> {
    let f = file(dir, path);
    if let Some(parent) = f.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = f.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec(p).map_err(std::io::Error::other)?)?;
    std::fs::rename(&tmp, &f)
}

/// Mixed In Key's "Energy N" cues: (bar, level).
pub fn energy_cues(cues: &[Option<Cue>], bar_of: impl Fn(f64) -> f64) -> Vec<(f64, u8)> {
    cues.iter()
        .flatten()
        .filter_map(|c| {
            let n: u8 = c.name.trim().strip_prefix("Energy")?.trim().parse().ok()?;
            Some((bar_of(c.secs), n.clamp(1, 10)))
        })
        .collect()
}

impl Profile {
    /// From a track's bars as the transition search sees them (see `Core::deck_bars`).
    pub fn new(path: &Path, bpm: f64, bars: &Bars, cue_bars: Vec<f64>, sections: &[Section]) -> Self {
        Profile {
            version: VERSION,
            stamp: stamp(path),
            bpm,
            bars: bars.clone(),
            cue_bars,
            sections: sections.iter().map(|s| (s.start, s.end, s.label.to_string())).collect(),
            stems: bars.stems.is_some(),
        }
    }

    /// From what preparing a track gives: its waveform, stems, grid, cues, energy and sections.
    #[allow(clippy::too_many_arguments)]
    pub fn build(path: &Path, waveform: &dj_library::Waveform, stem_wave: Option<&StemWave>, grid: BeatGrid, duration: f64, cues: &[Option<Cue>], energy: Option<u8>, sections: &[Section]) -> Option<Self> {
        let spb = grid.seconds_per_beat();
        let bar_of = |t: f64| (t - grid.anchor_secs) / (4.0 * spb);
        let cue_bars: Vec<f64> = cues.iter().flatten().map(|c| bar_of(c.secs)).collect();
        let bars = Bars::new(waveform, stem_wave, grid.anchor_secs, spb, duration).with_phrase(phrase_offset(&cue_bars)).with_energy(&energy_cues(cues, bar_of), energy);
        (!bars.is_empty()).then(|| Self::new(path, grid.bpm, &bars, cue_bars, sections))
    }

    /// How much of the track has vocals (see [`Bars::vocal_share`]).
    pub fn vocals(&self) -> Option<u8> {
        self.bars.vocal_share()
    }

    /// The track bar by bar, ready for the transition search.
    pub fn bars(&self) -> Bars {
        let sections: Vec<Section> = self.sections.iter().filter_map(|(a, b, l)| Some(Section { start: *a, end: *b, label: stems_core::structure::label(l)? })).collect();
        self.bars.clone().finished().with_sections(&sections)
    }
}

/// What preparing a track did.
#[derive(Clone, Debug, PartialEq)]
pub enum Prepared {
    /// Already prepared (as far as the settings allow).
    Already,
    /// Done: whether stems were separated now, whether it has sections, its key if it was
    /// detected now (no key tag), its detected grid if the heard beats corrected its tempo, and
    /// how much of it has vocals (with stems).
    Done { separated: bool, sections: bool, key: Option<dj_library::key::Key>, grid: Option<dj_engine::track::BeatGrid>, vocals: Option<u8> },
}

/// Prepare a track for mixing ahead of time, as loading it on a deck would: its grid (checked
/// against the audio and the beats All-In-One hears), stems (with HT-Demucs when there's a model;
/// kept), sections and beats (kept) and its mix profile. Already prepared tracks are skipped.
pub fn prepare_track(
    path: &Path,
    overrides: Option<&dj_library::TrackOverrides>,
    demucs: Option<&Path>,
    dir: &Path,
    threads: usize,
    caches: &mut (stems::DemucsCache, stems::StructureCache),
    progress: &mut dyn FnMut(f32) -> bool,
) -> Result<Prepared, String> {
    if read(dir, path).is_some_and(|p| p.stems || demucs.is_none()) {
        return Ok(Prepared::Already);
    }
    let detect = dj_library::DetectOptions { prefer_detected_cues: false, cues_from_mix: true };
    let lt = dj_library::load(path, overrides, false, detect).map_err(|e| e.to_string())?;
    let audio = &lt.track.audio;
    let Some(mut grid) = lt.track.grid else { return Err("no beat grid (no BPM found)".into()) };
    let (stems, separated) = stems::stems_for(audio, path, demucs, threads, &mut caches.0, dir, progress)?;
    let found = stems.as_ref().map(|s| stems::structure_for(audio, s, dir, (threads / 2).max(1), &mut caches.1)).unwrap_or_default();
    // The grid as a deck would play it: corrected from the heard beats (unless it's yours or
    // follows a tempo map).
    let mine = lt.grid_source == Some(dj_library::GridSource::User);
    let heard: Vec<(f64, f32)> = found.beats.iter().map(|b| (b.secs, b.downbeat)).collect();
    // A detected tempo the heard beats steadily disagree with: theirs.
    let retempo = (lt.grid_source == Some(dj_library::GridSource::Detected) && lt.track.tempo_map.is_none())
        .then(|| dj_library::analysis::tempo_from_beats(&grid, &heard))
        .flatten();
    if let Some(g) = retempo {
        grid = g;
    } else if !mine && lt.track.tempo_map.is_none() {
        if let Some(check) = dj_library::analysis::check_beats(|t| grid.beat_at(t), grid.seconds_per_beat(), &heard) {
            let (half, bars, _) = dj_library::analysis::grid_shift(&check, |t| grid.beat_at(t), &lt.track.cues);
            grid.anchor_secs += (half + bars) * grid.seconds_per_beat();
        }
    }
    let wave = stems.as_ref().map(|s| StemWave::compute(&audio.frames, audio.sample_rate, s));
    let profile = Profile::build(path, &lt.waveform, wave.as_ref(), grid, audio.duration_secs(), &lt.track.cues, lt.tags.energy, &found.sections).ok_or("too short")?;
    write(dir, path, &profile).map_err(|e| format!("writing the profile: {e}"))?;
    Ok(Prepared::Done { separated, sections: !found.sections.is_empty(), key: lt.detected.key.map(|(k, _)| k), grid: retempo, vocals: profile.vocals() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_profile_comes_back_as_it_was_made() {
        let dir = std::env::temp_dir().join(format!("dj-profile-{}", std::process::id()));
        let track = dir.join("song.wav");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&track, b"not really audio").unwrap();
        // 32 bars: drums and other throughout, bass from bar 8, vocals 12-24.
        let lv = |r: (usize, usize)| (0..32).map(|k| if (r.0..r.1).contains(&k) { 0.5 } else { 0.0 }).collect::<Vec<f32>>();
        let bars = Bars { start: 0.25, bar_secs: 2.0, stems: Some([lv((0, 32)), lv((8, 32)), lv((12, 24)), lv((0, 32))]), bands: [lv((0, 32)), lv((0, 32)), lv((0, 32))], loud: lv((0, 32)), ..Default::default() }.finished().with_phrase(4);
        let sections = [Section { start: 0.25, end: 16.25, label: "intro" }, Section { start: 16.25, end: 64.25, label: "chorus" }];
        let p = Profile::new(&track, 120.0, &bars, vec![0.0, 8.0], &sections);
        write(&dir, &track, &p).unwrap();
        let back = read(&dir, &track).expect("read back");
        let b = back.bars();
        assert_eq!((b.len(), b.intro_end(), b.phrase, b.label(10)), (32, 8, 4, "chorus"));
        assert!(b.starts_section(8));
        assert!(back.stems && back.bpm == 120.0 && back.cue_bars == vec![0.0, 8.0]);
        assert_eq!(back.vocals(), Some(38), "vocals in 12 of its 32 bars");
        // The file changed: made again.
        std::fs::write(&track, b"other audio, other length").unwrap();
        assert!(read(&dir, &track).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Preparing a list of tracks in the background (the library's Prepare).
pub struct Preparer {
    state: std::sync::Arc<std::sync::Mutex<PrepState>>,
    /// Keys and corrected grids found, for the app to keep.
    found: std::sync::Arc<std::sync::Mutex<Vec<Found>>>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// What preparing a track found for the app to keep: its key (no key tag), its detected grid
/// when the heard beats corrected its tempo, and how much of it has vocals.
#[derive(Clone, Debug)]
pub struct Found {
    pub path: PathBuf,
    pub key: Option<dj_library::key::Key>,
    pub grid: Option<dj_engine::track::BeatGrid>,
    pub vocals: Option<u8>,
}

/// How far a [`Preparer`] is.
#[derive(Clone, Debug, Default)]
pub struct PrepState {
    pub total: usize,
    /// Tracks finished (prepared now, already prepared, or failed).
    pub done: usize,
    pub prepared: usize,
    pub failed: usize,
    /// The track being prepared, and how far its stems are (0..1).
    pub current: String,
    pub fraction: f32,
    pub finished: bool,
}

impl Preparer {
    /// Start preparing `tracks` (with their saved overrides) on a background thread, gently: half
    /// the cores, so playing isn't disturbed.
    pub fn start(tracks: Vec<(PathBuf, Option<dj_library::TrackOverrides>)>, demucs: Option<PathBuf>, dir: PathBuf) -> Self {
        use std::sync::atomic::Ordering;
        let state = std::sync::Arc::new(std::sync::Mutex::new(PrepState { total: tracks.len(), ..Default::default() }));
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let found = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (st, sp, ks) = (state.clone(), stop.clone(), found.clone());
        let threads = std::thread::available_parallelism().map(|n| (n.get() / 2).max(1)).unwrap_or(2);
        std::thread::Builder::new()
            .name("prepare".into())
            .spawn(move || {
                let mut caches = (stems::DemucsCache::default(), stems::StructureCache::default());
                let set = |f: &dyn Fn(&mut PrepState)| {
                    if let Ok(mut s) = st.lock() {
                        f(&mut s);
                    }
                };
                for (path, overrides) in tracks {
                    if sp.load(Ordering::Relaxed) {
                        break;
                    }
                    let name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                    set(&|s| {
                        s.current = name.clone();
                        s.fraction = 0.0;
                    });
                    let mut progress = |p: f32| {
                        set(&|s| s.fraction = p);
                        !sp.load(Ordering::Relaxed)
                    };
                    let r = prepare_track(&path, overrides.as_ref(), demucs.as_deref(), &dir, threads, &mut caches, &mut progress);
                    if let Err(e) = &r {
                        eprintln!("prepare {}: {e}", path.display());
                    }
                    // (Prepared before: how much of it has vocals, from its profile.)
                    let found = match &r {
                        Ok(Prepared::Done { key, grid, vocals, .. }) => Found { path: path.clone(), key: *key, grid: *grid, vocals: *vocals },
                        Ok(Prepared::Already) => Found { path: path.clone(), key: None, grid: None, vocals: read(&dir, &path).and_then(|p| p.vocals()) },
                        Err(_) => Found { path: path.clone(), key: None, grid: None, vocals: None },
                    };
                    if found.key.is_some() || found.grid.is_some() || found.vocals.is_some() {
                        if let Ok(mut ks) = ks.lock() {
                            ks.push(found);
                        }
                    }
                    set(&|s| {
                        s.done += 1;
                        match &r {
                            Ok(Prepared::Done { .. }) => s.prepared += 1,
                            Ok(Prepared::Already) => {}
                            Err(_) => s.failed += 1,
                        }
                    });
                }
                set(&|s| s.finished = true);
            })
            .expect("spawn prepare");
        Self { state, stop, found }
    }

    /// What was found since the last call.
    pub fn take_found(&self) -> Vec<Found> {
        self.found.lock().map(|mut k| std::mem::take(&mut *k)).unwrap_or_default()
    }

    pub fn state(&self) -> PrepState {
        self.state.lock().map(|s| s.clone()).unwrap_or_default()
    }

    /// Stop after (or in the middle of) the current track.
    pub fn stop(&self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}
