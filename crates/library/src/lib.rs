//! Track loading for dj.
//!
//! BPM, key, energy and cue points come from your tags (Mixed In Key / Serato markers) when
//! they are there. For tracks without them, [`analysis`] detects the tempo and beat grid, a rough
//! energy level and cue points (key is never detected).

pub mod analysis;
pub mod crates;
pub mod key;
pub mod keydetect;
pub mod loudness;
pub mod scan;
pub mod tags;
pub mod waveform;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use dj_engine::dsp::db_to_gain;
use dj_engine::track::{AudioData, BeatGrid, Cue, CueKind, Track, NUM_HOT_CUES};
use serde::{Deserialize, Serialize};
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

pub use key::Key;
pub use tags::Tags;
pub use waveform::Waveform;

pub const AUDIO_EXTENSIONS: &[&str] = &["mp3", "flac", "wav", "aif", "aiff", "m4a", "mp4", "aac", "ogg", "opus", "alac"];

pub fn is_audio_file(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| AUDIO_EXTENSIONS.iter().any(|x| x.eq_ignore_ascii_case(e)))
        .unwrap_or(false)
}

/// Decode a file to stereo f32 at its native sample rate.
pub fn decode(path: &Path) -> Result<AudioData> {
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let mut format = symphonia::default::get_probe()
        .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
        .map_err(|e| anyhow!("unsupported file: {e}"))?;
    let track = format.default_track(TrackType::Audio).ok_or_else(|| anyhow!("no audio track"))?;
    let track_id = track.id;
    let params = track
        .codec_params
        .as_ref()
        .and_then(|p| p.audio())
        .ok_or_else(|| anyhow!("no audio codec parameters"))?
        .clone();
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&params, &AudioDecoderOptions::default())
        .map_err(|e| anyhow!("no decoder: {e}"))?;

    let mut frames: Vec<[f32; 2]> = Vec::new();
    let mut sample_rate = params.sample_rate.unwrap_or(0);
    let mut scratch: Vec<f32> = Vec::new();
    loop {
        let packet = match format.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break,
            Err(SymError::ResetRequired) => break,
            Err(SymError::IoError(_)) => break,
            Err(e) => return Err(anyhow!("demux error: {e}")),
        };
        if packet.track_id != track_id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(buf) => {
                let spec = buf.spec();
                sample_rate = spec.rate();
                let ch = spec.channels().count().max(1);
                scratch.resize(buf.samples_interleaved(), 0.0);
                buf.copy_to_slice_interleaved(&mut scratch);
                frames.reserve(scratch.len() / ch);
                for f in scratch.chunks_exact(ch) {
                    frames.push(if ch == 1 { [f[0], f[0]] } else { [f[0], f[1]] });
                }
            }
            Err(SymError::DecodeError(_)) => continue,
            Err(e) => return Err(anyhow!("decode error: {e}")),
        }
    }
    if frames.is_empty() || sample_rate == 0 {
        return Err(anyhow!("no audio decoded"));
    }
    Ok(AudioData { sample_rate, frames })
}

/// Track length from container headers only (no decoding).
pub fn probe_duration(path: &Path) -> Option<f64> {
    let file = std::fs::File::open(path).ok()?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let format = symphonia::default::get_probe().probe(&hint, mss, FormatOptions::default(), MetadataOptions::default()).ok()?;
    let track = format.default_track(TrackType::Audio)?;
    let frames = track.num_frames?;
    let sr = track.codec_params.as_ref()?.audio()?.sample_rate?;
    Some(frames as f64 / sr as f64)
}

/// Where the beat grid of a loaded track came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum GridSource {
    /// Serato BeatGrid tag.
    Tag,
    /// BPM tag, anchored on the first cue point.
    FirstCue,
    /// BPM tag, anchored where the kick drums fall.
    Aligned,
    /// User-adjusted and saved.
    User,
    /// Detected from the audio (no BPM tag).
    Detected,
}

/// Serato's (and so Mixed In Key's) times in an MP3 are one MP3 frame (1152 samples) later than
/// where a decoder that skips the encoder's lead-in plays the audio, as dj's does: 26 ms at 44.1
/// kHz, 24 ms at 48 kHz (measured by Mixxx, `SeratoTags::guessTimingOffsetMillis`). Move cues,
/// loops and the Serato beat grid back by that much, so they land on the music.
pub fn serato_timing(tags: &mut Tags, path: &Path, sample_rate: u32) {
    let mp3 = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("mp3"));
    if !mp3 || sample_rate == 0 {
        return;
    }
    let shift = 1152.0 / sample_rate as f64;
    for c in &mut tags.cues {
        c.secs = (c.secs - shift).max(0.0);
        if let Some(e) = c.end_secs.as_mut() {
            *e = (*e - shift).max(0.0);
        }
    }
    if let Some(g) = tags.grid.as_mut() {
        g.anchor_secs -= shift;
    }
    if let Some(m) = tags.tempo_map.as_mut() {
        m.shift_secs(-shift);
    }
}

/// Mixed In Key puts its cues on phrase starts, so on downbeats. When most hot cues sit the same
/// whole number of beats into the grid's bars, the grid's first beat isn't a downbeat (a Serato
/// grid can start on any beat): move it there so the bars are the track's. Returns the beats moved.
pub fn downbeat_from_cues(grid: &mut BeatGrid, cues: &[Option<Cue>]) -> Option<f64> {
    if grid.bpm <= 0.0 {
        return None;
    }
    let spb = 60.0 / grid.bpm;
    // (A cue at the very start may be one moved there from just before it.)
    let beats: Vec<f64> = cues
        .iter()
        .flatten()
        .filter(|c| c.kind == CueKind::Hot && c.secs > 0.1)
        .filter_map(|c| {
            let b = (c.secs - grid.anchor_secs) / spb;
            let r = b.round();
            ((b - r).abs() < 0.15).then(|| r.rem_euclid(4.0))
        })
        .collect();
    let best = (1..4).map(|k| k as f64).max_by_key(|k| beats.iter().filter(|b| *b == k).count())?;
    let agree = beats.iter().filter(|b| **b == best).count();
    if agree < 2 || agree * 2 <= beats.len() {
        return None;
    }
    grid.anchor_secs += best * spb;
    Some(best)
}

/// Cues sitting on a beat of `old` move to that beat of `new` (a corrected grid): they were set
/// on the tagger's beats, which the correction moved; cues between beats stay.
pub fn follow_grid(cues: &mut [Option<Cue>], old: &BeatGrid, new: &BeatGrid) {
    let moved = |t: f64| {
        let b = old.beat_at(t);
        if (b - b.round()).abs() < 0.06 {
            new.secs_at_beat(b.round()).max(0.0)
        } else {
            t
        }
    };
    for c in cues.iter_mut().flatten() {
        c.secs = moved(c.secs);
        if let Some(e) = c.end_secs.as_mut() {
            *e = moved(*e);
        }
    }
}

/// Choose a beat grid from tags, never estimating the tempo itself.
pub fn grid_for(tags: &Tags, audio: &AudioData) -> Option<(BeatGrid, GridSource)> {
    if let Some(g) = tags.grid {
        return Some((g, GridSource::Tag));
    }
    let bpm = tags.bpm?;
    if let Some(c) = tags.cues.iter().filter(|c| c.kind == CueKind::Hot).map(|c| c.secs).reduce(f64::min) {
        return Some((BeatGrid { bpm, anchor_secs: c }, GridSource::FirstCue));
    }
    Some((BeatGrid { bpm, anchor_secs: waveform::align_beats(audio, bpm) }, GridSource::Aligned))
}

#[derive(Clone)]
pub struct LoadedTrack {
    pub path: PathBuf,
    pub track: Arc<Track>,
    pub tags: Tags,
    pub grid_source: Option<GridSource>,
    pub waveform: Arc<Waveform>,
    pub detected: Detected,
    /// Integrated loudness (LUFS), measured at load.
    pub loudness: Option<f64>,
}

/// Loudness matching brings every track to this (LUFS), within ±12 dB.
pub const LOUDNESS_TARGET: f64 = -12.0;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

pub fn next_track_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

/// Overrides persisted by the app (grid edits, cues set in Perform, detected values).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TrackOverrides {
    pub grid: Option<BeatGrid>,
    /// `grid` was detected, not set by hand.
    pub grid_detected: bool,
    pub cues: Option<Vec<Option<Cue>>>,
    pub locked: bool,
    /// Estimated energy (tracks without an energy tag).
    pub energy: Option<u8>,
    /// Detected key (tracks without a key tag).
    pub key: Option<key::Key>,
}

/// What `load` should detect.
#[derive(Clone, Copy, Debug, Default)]
pub struct DetectOptions {
    /// Detect cue points even when the tags have some (the user prefers dj's).
    pub prefer_detected_cues: bool,
    /// Detect cue points now, from the mix (when no stems will come to do it better).
    pub cues_from_mix: bool,
}

/// What `load` detected (to be saved), and what is still to do.
#[derive(Clone, Debug, Default)]
pub struct Detected {
    /// The grid was detected now.
    pub grid: bool,
    /// Energy was estimated now.
    pub energy: Option<u8>,
    /// Cue points were detected now (from the mix).
    pub cues: bool,
    /// Cue points are wanted but were not detected yet (do it once stems are ready).
    pub cues_pending: bool,
    /// The tag grid was off the audio and corrected: (ms moved, BPM change).
    pub grid_fix: Option<(f64, f64)>,
    /// The tempo changes through the track: it plays by a measured tempo map.
    pub tempo_map: bool,
    /// The key was detected now (no key tag), and how sure that is (0..1).
    pub key: Option<(key::Key, f64)>,
}

/// Decode + read tags + build waveform. Runs on a worker thread.
pub fn load(path: &Path, overrides: Option<&TrackOverrides>, match_loudness: bool, detect: DetectOptions) -> Result<LoadedTrack> {
    let audio = decode(path)?;
    let mut tags = tags::read(path).unwrap_or_default();
    serato_timing(&mut tags, path, audio.sample_rate);
    let mut detected = Detected::default();
    let (mut grid, mut source) = match grid_for(&tags, &audio) {
        Some((g, s)) => (Some(g), Some(s)),
        None => (None, None),
    };
    let mut cues: [Option<Cue>; NUM_HOT_CUES] = Default::default();
    // Hot cues keep their Serato slot; loops fill remaining pads.
    let mut loops = Vec::new();
    for (c, &slot) in tags.cues.iter().zip(&tags.cue_slots) {
        if c.kind == CueKind::Hot && (slot as usize) < NUM_HOT_CUES {
            cues[slot as usize] = Some(c.clone());
        } else {
            loops.push(c.clone());
        }
    }
    for l in loops {
        if let Some(free) = cues.iter_mut().find(|c| c.is_none()) {
            *free = Some(l);
        }
    }
    if let Some(o) = overrides {
        if let Some(g) = o.grid {
            grid = Some(g);
            source = Some(if o.grid_detected { GridSource::Detected } else { GridSource::User });
        }
        if let Some(c) = &o.cues {
            // Cues saved before the MP3 timing fix kept Serato's late times: a saved cue at exactly
            // a tag cue's uncorrected time is that cue, so it takes the corrected time.
            let shift = if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("mp3")) && audio.sample_rate > 0 {
                1152.0 / audio.sample_rate as f64
            } else {
                0.0
            };
            for (i, v) in c.iter().enumerate().take(NUM_HOT_CUES) {
                let mut v = v.clone();
                if let Some(cue) = v.as_mut() {
                    if let Some(t) = tags.cues.iter().find(|t| shift > 0.0 && (t.secs + shift - cue.secs).abs() < 0.0005) {
                        cue.secs = t.secs;
                        cue.end_secs = t.end_secs;
                    }
                }
                cues[i] = v;
            }
        }
        if tags.energy.is_none() {
            tags.energy = o.energy;
        }
        if tags.key.is_none() {
            tags.key = o.key;
        }
    }
    // No key tag (nor one detected before): from the audio.
    if tags.key.is_none() {
        if let Some((k, sure)) = keydetect::detect(&audio) {
            tags.key = Some(k);
            detected.key = Some((k, sure));
        }
    }

    // A Serato grid with several markers: the tempo changes, deliberately (your own grid wins).
    let mut tempo_map = if source == Some(GridSource::Tag) { tags.tempo_map.clone() } else { None };
    // A tag grid whose first beat isn't a downbeat: the cues say where the bars start.
    if let (Some(g), Some(GridSource::Tag | GridSource::FirstCue)) = (grid.as_mut(), source) {
        if let (Some(beats), Some(m)) = (downbeat_from_cues(g, &cues), tempo_map.as_mut()) {
            m.shift_beats(-beats);
        }
    }

    // Detection for what the tags (and saved values) do not give.
    let user_cues = overrides.is_some_and(|o| o.cues.is_some());
    let want_cues = !user_cues && (detect.prefer_detected_cues || cues.iter().all(|c| c.is_none()));
    // A grid from tags (or anchored by dj on a tagged tempo) is checked against the audio.
    let check_grid = grid.is_some() && tempo_map.is_none() && matches!(source, Some(GridSource::Tag | GridSource::FirstCue | GridSource::Aligned));
    let need = check_grid || grid.is_none() || tags.energy.is_none() || (want_cues && detect.cues_from_mix);
    let env = need.then(|| analysis::Envelope::compute(&audio));
    if let (true, Some(g), Some(env)) = (check_grid, grid, &env) {
        if let Some(fixed) = analysis::refine_grid(env, &g) {
            follow_grid(&mut cues, &g, &fixed);
            detected.grid_fix = Some(((fixed.anchor_secs - g.anchor_secs) * 1000.0, fixed.bpm - g.bpm));
            grid = Some(fixed);
        }
    }
    let fresh = check_grid || grid.is_none();
    if let (None, Some(env)) = (grid, &env) {
        if let Some(t) = analysis::detect_grid(env) {
            grid = Some(t.grid);
            source = Some(GridSource::Detected);
            detected.grid = true;
        }
    }
    // A tempo that really changes (a live drummer, a speed-up): a map through the beats, for a
    // grid checked or detected just now. Cues on the steady grid's beats move to the map's.
    if let (true, true, Some(g), Some(env)) = (fresh, tempo_map.is_none(), grid, &env) {
        if let Some(m) = analysis::tempo_map(env, &g) {
            for c in cues.iter_mut().flatten() {
                let b = g.beat_at(c.secs);
                if (b - b.round()).abs() < 0.06 {
                    c.secs = m.secs_at_beat(b.round()).max(0.0);
                }
            }
            detected.tempo_map = true;
            tempo_map = Some(m);
        }
    }
    if let (None, Some(env)) = (tags.energy, &env) {
        let e = analysis::estimate_energy(&audio, env, grid.map(|g| g.bpm));
        tags.energy = Some(e);
        detected.energy = Some(e);
    }
    if want_cues {
        match (grid, &env) {
            (Some(g), Some(env)) if detect.cues_from_mix => {
                let found = analysis::detect_cues(&audio, env, &g, None);
                if !found.is_empty() {
                    cues = Default::default();
                    for (slot, c) in cues.iter_mut().zip(found) {
                        *slot = Some(c);
                    }
                    detected.cues = true;
                }
            }
            (Some(_), _) => detected.cues_pending = true,
            _ => {}
        }
    }
    let waveform = Arc::new(Waveform::build(&audio));
    let loudness = loudness::integrated(&audio);
    let gain = match loudness {
        Some(l) if match_loudness => db_to_gain(((LOUDNESS_TARGET - l).clamp(-12.0, 12.0)) as f32),
        _ => 1.0,
    };
    let track = Arc::new(Track { id: next_track_id(), audio, grid, tempo_map, gain, cues });
    Ok(LoadedTrack { path: path.to_path_buf(), track, tags, grid_source: source, waveform, detected, loudness })
}

/// Decode a short sample for a sampler lane (max 15 s, like the original).
pub fn load_sample(path: &Path) -> Result<Arc<AudioData>> {
    let mut a = decode(path)?;
    let max = a.sample_rate as usize * 15;
    a.frames.truncate(max);
    Ok(Arc::new(a))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_track_without_a_key_tag_gets_its_key_from_the_audio() {
        // A minor, as a 16-bit WAV without tags.
        let song = keydetect::tests::song(&[[33, 57, 60, 64], [38, 57, 62, 65], [40, 56, 59, 64], [33, 57, 60, 64]]);
        let sr = song.sample_rate;
        let pcm: Vec<u8> = song.frames.iter().flat_map(|f| [f[0], f[1]]).flat_map(|v| ((v * 32000.0) as i16).to_le_bytes()).collect();
        let mut fmt = Vec::new();
        for b in [&1u16.to_le_bytes()[..], &2u16.to_le_bytes(), &sr.to_le_bytes(), &(sr * 4).to_le_bytes(), &4u16.to_le_bytes(), &16u16.to_le_bytes()] {
            fmt.extend_from_slice(b);
        }
        let chunk = |id: &[u8], b: &[u8]| [id, &(b.len() as u32).to_le_bytes(), b].concat();
        let body = [b"WAVE".to_vec(), chunk(b"fmt ", &fmt), chunk(b"data", &pcm)].concat();
        let wav = [b"RIFF".to_vec(), (body.len() as u32).to_le_bytes().to_vec(), body].concat();
        let dir = std::env::temp_dir().join(format!("dj-key-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("a-minor.wav");
        std::fs::write(&p, &wav).unwrap();

        let am = key::Key { pitch: 9, minor: true };
        let t = load(&p, None, false, DetectOptions::default()).unwrap();
        assert_eq!(t.tags.key, Some(am));
        assert_eq!(t.detected.key.map(|(k, _)| k), Some(am), "detected now: to be saved");
        // Saved: the next load takes it as it was (and doesn't detect again).
        let saved = TrackOverrides { key: Some(key::Key { pitch: 0, minor: false }), ..Default::default() };
        let t = load(&p, Some(&saved), false, DetectOptions::default()).unwrap();
        assert_eq!((t.tags.key, t.detected.key), (saved.key, None));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn decodes_wav_and_reads_id3_chunk() {
        // 1 s of 48 kHz stereo 16-bit with a MIK-style ID3 chunk.
        let sr = 48000u32;
        let mut pcm = Vec::new();
        for i in 0..sr {
            let v = ((i as f32 / sr as f32 * 440.0 * std::f32::consts::TAU).sin() * 16000.0) as i16;
            pcm.extend_from_slice(&v.to_le_bytes());
            pcm.extend_from_slice(&v.to_le_bytes());
        }
        let id3 = tags::tests::mik_id3();
        let mut fmt = Vec::new();
        fmt.extend_from_slice(&1u16.to_le_bytes());
        fmt.extend_from_slice(&2u16.to_le_bytes());
        fmt.extend_from_slice(&sr.to_le_bytes());
        fmt.extend_from_slice(&(sr * 4).to_le_bytes());
        fmt.extend_from_slice(&4u16.to_le_bytes());
        fmt.extend_from_slice(&16u16.to_le_bytes());
        let chunk = |id: &[u8], b: &[u8]| {
            let mut v = id.to_vec();
            v.extend_from_slice(&(b.len() as u32).to_le_bytes());
            v.extend_from_slice(b);
            if b.len() % 2 == 1 {
                v.push(0);
            }
            v
        };
        let body = [b"WAVE".to_vec(), chunk(b"fmt ", &fmt), chunk(b"data", &pcm), chunk(b"id3 ", &id3)].concat();
        let mut wav = b"RIFF".to_vec();
        wav.extend_from_slice(&(body.len() as u32).to_le_bytes());
        wav.extend_from_slice(&body);
        let dir = std::env::temp_dir().join(format!("dj-lib-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.wav");
        std::fs::write(&p, &wav).unwrap();

        let t = load(&p, None, false, DetectOptions::default()).unwrap();
        assert_eq!(t.track.audio.sample_rate, 48000);
        assert_eq!(t.track.audio.frames.len(), 48000);
        assert_eq!(t.tags.key.unwrap().camelot(), "8A");
        assert_eq!(t.grid_source, Some(GridSource::Tag));
        assert_eq!(t.track.grid.unwrap().bpm, 124.0);
        assert_eq!(t.track.cues[0].as_ref().unwrap().name, "Intro");
        assert_eq!(t.track.cues[2].as_ref().unwrap().name, "Drop");
        // Loop fills the first free pad.
        assert_eq!(t.track.cues[1].as_ref().unwrap().kind, CueKind::Loop);
        assert!(t.waveform.len() > 90);
        assert!(!t.detected.grid && !t.detected.cues_pending, "tags win: nothing detected but energy");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn untagged_tracks_get_a_detected_grid_energy_and_cues() {
        // Intro (hats + pad) 16 bars, then the full groove 32 bars, at 126 BPM, no tags.
        let bars: Vec<_> = [vec![(false, true, false, true); 16], vec![(true, true, true, true); 32]].concat();
        let audio = analysis::tests::track(126.0, 0.25, &bars);
        let pcm: Vec<u8> = audio.frames.iter().flat_map(|f| f.iter().flat_map(|v| ((v.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes())).collect();
        let sr = audio.sample_rate;
        let mut fmt = Vec::new();
        fmt.extend_from_slice(&1u16.to_le_bytes());
        fmt.extend_from_slice(&2u16.to_le_bytes());
        fmt.extend_from_slice(&sr.to_le_bytes());
        fmt.extend_from_slice(&(sr * 4).to_le_bytes());
        fmt.extend_from_slice(&4u16.to_le_bytes());
        fmt.extend_from_slice(&16u16.to_le_bytes());
        let chunk = |id: &[u8], b: &[u8]| [id.to_vec(), (b.len() as u32).to_le_bytes().to_vec(), b.to_vec()].concat();
        let body = [b"WAVE".to_vec(), chunk(b"fmt ", &fmt), chunk(b"data", &pcm)].concat();
        let wav = [b"RIFF".to_vec(), (body.len() as u32).to_le_bytes().to_vec(), body].concat();
        let dir = std::env::temp_dir().join(format!("dj-detect-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("untagged.wav");
        std::fs::write(&p, &wav).unwrap();

        let t = load(&p, None, false, DetectOptions { cues_from_mix: true, ..Default::default() }).unwrap();
        assert_eq!(t.grid_source, Some(GridSource::Detected));
        assert!((t.track.grid.unwrap().bpm - 126.0).abs() < 0.02);
        assert!(t.detected.grid && t.detected.energy.is_some() && t.detected.cues);
        let names: Vec<&str> = t.track.cues.iter().flatten().map(|c| c.name.as_str()).collect();
        assert_eq!(names.first(), Some(&"Intro"), "{names:?}");
        assert!(names.contains(&"Drop"), "{names:?}");

        // With stems coming, cue detection waits for them.
        let later = load(&p, None, false, DetectOptions::default()).unwrap();
        assert!(later.detected.cues_pending && later.track.cues.iter().all(|c| c.is_none()));
        // Saved values come back without detecting again.
        let saved = TrackOverrides { grid: t.track.grid, grid_detected: true, energy: t.detected.energy, cues: Some(t.track.cues.to_vec()), ..Default::default() };
        let again = load(&p, Some(&saved), false, DetectOptions::default()).unwrap();
        assert_eq!(again.grid_source, Some(GridSource::Detected));
        assert!(!again.detected.grid && again.detected.energy.is_none() && !again.detected.cues_pending);
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod downbeat_tests {
    use super::*;

    fn hot(secs: f64) -> Option<Cue> {
        Some(Cue { kind: CueKind::Hot, secs, end_secs: None, name: String::new(), color: [0; 3] })
    }

    #[test]
    fn cues_two_beats_into_the_bars_move_the_downbeat() {
        // Too Good: a Serato grid starting on beat 3; Mixed In Key's cues on the real downbeats.
        let mut g = BeatGrid { bpm: 118.0, anchor_secs: 0.910 };
        let spb = 60.0 / 118.0;
        let cues = [hot(0.0), hot(16.16), hot(73.11), hot(89.38), hot(154.47), hot(187.01), hot(203.28), hot(219.55)];
        assert_eq!(downbeat_from_cues(&mut g, &cues), Some(2.0));
        assert!((g.anchor_secs - (0.910 + 2.0 * spb)).abs() < 1e-9);
        for c in cues.iter().flatten().skip(1) {
            let bar = (c.secs - g.anchor_secs) / (4.0 * spb);
            assert!((bar - bar.round()).abs() < 0.03, "{} is on a bar line now ({bar})", c.secs);
        }
        // Cues already on bar lines (or not agreeing) leave the grid alone.
        let mut g = BeatGrid { bpm: 122.0, anchor_secs: 0.38 };
        let on_bars = [hot(0.38), hot(31.86), hot(47.6), hot(165.65)];
        assert_eq!(downbeat_from_cues(&mut g, &on_bars), None);
        let mut g = BeatGrid { bpm: 120.0, anchor_secs: 0.0 };
        assert_eq!(downbeat_from_cues(&mut g, &[hot(2.5), hot(4.0), hot(9.0)]), None, "no majority");
    }
}

#[cfg(test)]
mod serato_timing_tests {
    use super::*;

    #[test]
    fn mp3_serato_times_move_back_one_frame() {
        let cue = |secs| Cue { kind: CueKind::Hot, secs, end_secs: Some(secs + 4.0), name: String::new(), color: [0; 3] };
        let mut t = Tags { cues: vec![cue(10.0), cue(0.01)], grid: Some(BeatGrid { bpm: 128.0, anchor_secs: 0.5 }), ..Default::default() };
        serato_timing(&mut t, Path::new("/m/a.MP3"), 44100);
        let d = 1152.0 / 44100.0;
        assert!((t.cues[0].secs - (10.0 - d)).abs() < 1e-9 && (t.cues[0].end_secs.unwrap() - (14.0 - d)).abs() < 1e-9);
        assert_eq!(t.cues[1].secs, 0.0, "never before the start");
        assert!((t.grid.unwrap().anchor_secs - (0.5 - d)).abs() < 1e-9);
        assert!((d * 1000.0 - 26.1).abs() < 0.1);
        // 48 kHz: 24 ms. Other formats: unchanged.
        let mut t = Tags { cues: vec![cue(10.0)], ..Default::default() };
        serato_timing(&mut t, Path::new("/m/a.mp3"), 48000);
        assert!((t.cues[0].secs - 9.976).abs() < 1e-9);
        let mut t = Tags { cues: vec![cue(10.0)], ..Default::default() };
        serato_timing(&mut t, Path::new("/m/a.flac"), 44100);
        assert_eq!(t.cues[0].secs, 10.0);
    }
}
