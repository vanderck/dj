//! Track loading for dj.
//!
//! BPM, key, energy and cue points come from your tags (Mixed In Key / Serato markers) when
//! they are there. For tracks without them, [`analysis`] detects the tempo and beat grid, a rough
//! energy level and cue points (key is never detected).

pub mod analysis;
pub mod key;
pub mod scan;
pub mod tags;
pub mod waveform;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use collective_engine::dsp::db_to_gain;
use collective_engine::track::{AudioData, BeatGrid, Cue, CueKind, Track, NUM_HOT_CUES};
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
}

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
}

/// Decode + read tags + build waveform. Runs on a worker thread.
pub fn load(path: &Path, overrides: Option<&TrackOverrides>, use_replaygain: bool, detect: DetectOptions) -> Result<LoadedTrack> {
    let audio = decode(path)?;
    let mut tags = tags::read(path).unwrap_or_default();
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
            for (i, v) in c.iter().enumerate().take(NUM_HOT_CUES) {
                cues[i] = v.clone();
            }
        }
        if tags.energy.is_none() {
            tags.energy = o.energy;
        }
    }

    // Detection for what the tags (and saved values) do not give.
    let user_cues = overrides.is_some_and(|o| o.cues.is_some());
    let want_cues = !user_cues && (detect.prefer_detected_cues || cues.iter().all(|c| c.is_none()));
    let need = grid.is_none() || tags.energy.is_none() || (want_cues && detect.cues_from_mix);
    let env = need.then(|| analysis::Envelope::compute(&audio));
    if let (None, Some(env)) = (grid, &env) {
        if let Some(t) = analysis::detect_grid(env) {
            grid = Some(t.grid);
            source = Some(GridSource::Detected);
            detected.grid = true;
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
    let gain = if use_replaygain { tags.replaygain_db.map(db_to_gain).unwrap_or(1.0) } else { 1.0 };
    let track = Arc::new(Track { id: next_track_id(), audio, grid, gain, cues });
    Ok(LoadedTrack { path: path.to_path_buf(), track, tags, grid_source: source, waveform, detected })
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
        let dir = std::env::temp_dir().join(format!("collective-lib-{}", std::process::id()));
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
        let dir = std::env::temp_dir().join(format!("collective-detect-{}", std::process::id()));
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
