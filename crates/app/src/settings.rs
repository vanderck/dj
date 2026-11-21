//! Persistent settings, MIDI mappings and per-track overrides.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use dj_engine::deck::TempoRange;
use dj_engine::mixer::{ExtStages, FaderCurve};
use dj_library::TrackOverrides;
use serde::{de::DeserializeOwned, Deserialize, Serialize};

pub fn config_dir() -> PathBuf {
    if let Ok(p) = std::env::var("DJ_CONFIG_DIR") {
        return PathBuf::from(p);
    }
    static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        let dir = dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("dj");
        // dj was called Collective: move its settings folder over once.
        if let Some(old) = legacy_config_dir() {
            if !dir.exists() && old.is_dir() {
                let _ = std::fs::rename(&old, &dir);
            }
        }
        dir
    })
    .clone()
}

/// The settings folder from when dj was called Collective.
fn legacy_config_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("collective"))
}

pub fn load_json<T: DeserializeOwned + Default>(path: &Path) -> T {
    std::fs::read_to_string(path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

pub fn save_json<T: Serialize>(path: &Path, v: &T) -> anyhow::Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(v)?)?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioSettings {
    pub device: Option<String>,
    pub sample_rate: Option<u32>,
    pub buffer_frames: Option<u32>,
    /// Stereo pair index for Master (0 = outputs 1/2).
    pub master_pair: u8,
    pub cue_enabled: bool,
    pub cue_pair: u8,
    /// Mono cue left / mono master right on the cue pair.
    pub split_cue: bool,
}

impl Default for AudioSettings {
    fn default() -> Self {
        Self { device: None, sample_rate: None, buffer_frames: Some(256), master_pair: 0, cue_enabled: false, cue_pair: 1, split_cue: false }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum KeyNotation {
    #[default]
    Camelot,
    Musical,
    OpenKey,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub audio: AudioSettings,
    pub fader_curve: FaderCurve,
    pub crossover_low_hz: f32,
    pub crossover_high_hz: f32,
    pub ext_stages: ExtStages,
    pub deck_count: usize,
    pub tempo_range: TempoRange,
    pub keylock_on_load: bool,
    pub tape_stop_ms: f64,
    pub default_loop_beats: f64,
    pub skip_beats: f64,
    pub load_at_first_cue: bool,
    /// Bring every track to the same loudness (measured at load), so a mix doesn't jump.
    pub match_loudness: bool,
    pub osc_enabled: bool,
    pub osc_port: u16,
    /// Address root. Set to "/Syndicate/" to reuse controller layouts made
    /// for the original application.
    pub osc_root: String,
    pub osc_feedback: bool,
    pub osc_feedback_host: String,
    pub osc_feedback_port: u16,
    pub midi_inputs: Vec<String>,
    pub midi_output: Option<String>,
    pub midi_clock_out: bool,
    pub vst3_enabled: bool,
    pub vst3_paths: Vec<PathBuf>,
    pub library_folders: Vec<PathBuf>,
    pub key_notation: KeyNotation,
    /// Separate every loaded track into stems in the background.
    pub stems_enabled: bool,
    /// HT-Demucs ONNX model (best quality, about 1-3 min per track; results become prepared stems).
    /// `None` = off. See `stems::demucs_files` for HT-Demucs FT.
    pub stems_demucs: Option<PathBuf>,
    /// Where prepared stems (Demucs, `--prepare-stems`) are kept; never cleaned up
    /// automatically. `None` = `stems-prepared` in the config folder.
    pub stems_prepared_dir: Option<PathBuf>,
    /// Find each track's sections (intro, verse, chorus, break, outro) from its stems with
    /// All-In-One, for the auto mix and the waveform.
    pub song_sections: bool,
    /// Mixing happens on an external (hardware) mixer: every deck goes to its own output pair,
    /// dj's volume / trim / EQ / filter and the mixer section are bypassed and hidden.
    pub external_mixer: bool,
    /// Whose cue points a track gets: Mixed In Key's tags (detection only for tracks without
    /// cues) or dj's detection for every track. Cues you set yourself are always kept.
    pub cue_points: CuePoints,
    /// Screen layout: big touch controls, or (with a MIDI controller) four full decks with
    /// slim cue pads. `Auto` picks Controller when a hardware MIDI input is connected.
    pub layout: LayoutMode,
    /// Tempo faders move in steps of this many percent (0 = continuous).
    pub tempo_step_pct: f32,
    /// Settings migrations done (see `migrate_from_collective`).
    pub version: u32,
    /// What a set hot cue pad does, on screen and on a controller: loop from the cue (at the
    /// deck's loop size) or jump to it.
    pub cue_pads: CuePadMode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum CuePoints {
    /// The tags' cue points (Mixed In Key); dj detects them only for tracks without any.
    #[default]
    Tags,
    /// dj detects the cue points of every track (from its stems).
    Detected,
}

/// How Mix blends two tracks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum MixStyle {
    /// dj assesses the two tracks for each mix (see `Core::assess`).
    #[default]
    Auto,
    /// Swap drums, bass, vocals and other one by one, with the stems.
    Stems,
    /// Bass cut on the EQ and swapped at the midpoint, with volume fades (with an external mixer:
    /// you fade on the mixer).
    Eq,
    /// No blend: the new track cuts in on the beat, from its drop cue if it has one.
    Drop,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum CuePadMode {
    /// Start a loop at the cue, of the deck's loop size.
    #[default]
    Loop,
    /// Jump to the cue and play on.
    Jump,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum LayoutMode {
    #[default]
    Auto,
    Touch,
    Controller,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            audio: AudioSettings::default(),
            fader_curve: FaderCurve::LinearDb,
            crossover_low_hz: 250.0,
            crossover_high_hz: 2500.0,
            ext_stages: ExtStages::default(),
            deck_count: 4,
            tempo_range: TempoRange::Pct10,
            keylock_on_load: true,
            tape_stop_ms: 25.0,
            default_loop_beats: 4.0,
            skip_beats: 16.0,
            load_at_first_cue: true,
            match_loudness: true,
            osc_enabled: true,
            osc_port: 9090,
            osc_root: "/dj/".into(),
            osc_feedback: false,
            osc_feedback_host: "127.0.0.1".into(),
            osc_feedback_port: 9096,
            midi_inputs: Vec::new(),
            midi_output: None,
            midi_clock_out: false,
            vst3_enabled: true,
            vst3_paths: Vec::new(),
            library_folders: dirs::audio_dir().into_iter().collect(),
            key_notation: KeyNotation::Camelot,
            stems_enabled: true,
            stems_demucs: None,
            stems_prepared_dir: None,
            song_sections: true,
            external_mixer: false,
            layout: LayoutMode::Auto,
            cue_points: CuePoints::Tags,
            cue_pads: CuePadMode::Loop,
            version: 1,
            tempo_step_pct: 0.0,
        }
    }
}

impl Settings {
    pub fn stems_prepared_dir(&self) -> PathBuf {
        self.stems_prepared_dir.clone().unwrap_or_else(|| config_dir().join("stems-prepared"))
    }

    pub fn path() -> PathBuf {
        config_dir().join("settings.json")
    }

    pub fn load() -> Self {
        let mut s: Self = load_json(&Self::path());
        s.migrate_from_collective();
        s
    }

    /// Settings saved by Collective: paths into its old settings folder (a downloaded model)
    /// now live in dj's, and its default OSC root becomes dj's.
    fn migrate_from_collective(&mut self) {
        // Once: the default loop was 16 beats; it is 4 (a 16 chosen afterwards stays).
        if self.version < 1 {
            if self.default_loop_beats == 16.0 {
                self.default_loop_beats = 4.0;
            }
            self.version = 1;
        }
        if self.osc_root.trim_matches('/').eq_ignore_ascii_case("collective") {
            self.osc_root = "/dj/".into();
        }
        let (Some(old), new) = (legacy_config_dir(), config_dir()) else { return };
        for p in [&mut self.stems_demucs, &mut self.stems_prepared_dir].into_iter().flatten() {
            if let Ok(rest) = p.strip_prefix(&old) {
                *p = new.join(rest);
            }
        }
    }

    pub fn save(&self) -> anyhow::Result<()> {
        save_json(&Self::path(), self)
    }
}

// ------------------------------------------------------------ MIDI mappings

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MidiKind {
    Note,
    Cc,
    PitchBend,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MidiSource {
    pub kind: MidiKind,
    /// 0..15
    pub channel: u8,
    pub number: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum MapMode {
    /// Value is scaled into the target's range.
    #[default]
    Absolute,
    /// Each press flips a switch.
    Toggle,
    /// Relative encoder (two's complement around 64); sent as a step.
    Relative,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MidiMapping {
    pub source: MidiSource,
    /// Address relative to the OSC root, e.g. "Deck/1/PlayPause".
    pub address: String,
    pub mode: MapMode,
    #[serde(default)]
    pub invert: bool,
    /// Send LED feedback for switches on the MIDI output.
    #[serde(default = "yes")]
    pub feedback: bool,
}

fn yes() -> bool {
    true
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MappingProfile {
    pub name: String,
    pub mappings: Vec<MidiMapping>,
    /// Computer keyboard shortcuts: key name → address.
    pub keys: Vec<(String, String)>,
}

impl MappingProfile {
    pub fn path() -> PathBuf {
        config_dir().join("mappings.json")
    }

    pub fn load() -> Self {
        let mut p: MappingProfile = load_json(&Self::path());
        if p.keys.is_empty() && p.mappings.is_empty() {
            p = Self::default_profile();
        }
        p
    }

    pub fn save(&self) -> anyhow::Result<()> {
        save_json(&Self::path(), self)
    }

    pub fn default_profile() -> Self {
        let k = |a: &str, b: &str| (a.to_string(), b.to_string());
        MappingProfile {
            name: "Default".into(),
            mappings: Vec::new(),
            keys: vec![
                k("Z", "Deck/1/PlayPause"),
                k("X", "Deck/1/Cue"),
                k("C", "Deck/1/TempoSync"),
                k("M", "Deck/2/PlayPause"),
                k("Comma", "Deck/2/Cue"),
                k("Period", "Deck/2/TempoSync"),
                k("Num1", "Deck/1/Cue1"),
                k("Num2", "Deck/1/Cue2"),
                k("Num3", "Deck/1/Cue3"),
                k("Num4", "Deck/1/Cue4"),
                k("Num7", "Deck/2/Cue1"),
                k("Num8", "Deck/2/Cue2"),
                k("Num9", "Deck/2/Cue3"),
                k("Num0", "Deck/2/Cue4"),
                k("Q", "Deck/1/Loop"),
                k("P", "Deck/2/Loop"),
                k("Space", "Deck/1/PlayPause"),
            ],
        }
    }
}

// ------------------------------------------------------------ per-track overrides

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TrackDb {
    pub tracks: HashMap<String, TrackOverrides>,
    pub play_counts: HashMap<String, u32>,
}

impl TrackDb {
    pub fn path() -> PathBuf {
        config_dir().join("tracks.json")
    }

    pub fn load() -> Self {
        load_json(&Self::path())
    }

    pub fn save(&self) -> anyhow::Result<()> {
        save_json(&Self::path(), self)
    }

    pub fn key(p: &Path) -> String {
        p.to_string_lossy().into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collective_settings_move_to_dj() {
        let old = legacy_config_dir().unwrap();
        let mut s = Settings {
            osc_root: "/Collective/".into(),
            stems_demucs: Some(old.join("models").join("htdemucs.onnx")),
            stems_prepared_dir: Some(PathBuf::from("/elsewhere/stems")),
            version: 0,
            default_loop_beats: 16.0,
            ..Default::default()
        };
        s.migrate_from_collective();
        assert_eq!(s.osc_root, "/dj/");
        // (Other tests point DJ_CONFIG_DIR elsewhere meanwhile, so only check the move.)
        let moved = s.stems_demucs.clone().unwrap();
        assert!(!moved.starts_with(&old) && moved.ends_with("models/htdemucs.onnx"), "{moved:?}");
        assert_eq!(s.stems_prepared_dir, Some(PathBuf::from("/elsewhere/stems")), "other paths stay");
        assert_eq!(s.default_loop_beats, 4.0, "old default loop moves to 4 once");
        s.default_loop_beats = 16.0;
        s.migrate_from_collective();
        assert_eq!(s.default_loop_beats, 16.0, "a 16 chosen afterwards stays");
        // A root the user chose stays.
        let mut s = Settings { osc_root: "/Syndicate/".into(), ..Default::default() };
        s.migrate_from_collective();
        assert_eq!(s.osc_root, "/Syndicate/");
    }
}
