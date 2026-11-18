//! Persistent settings, MIDI mappings and per-track overrides.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use collective_engine::deck::TempoRange;
use collective_engine::mixer::{ExtStages, FaderCurve};
use collective_library::TrackOverrides;
use serde::{de::DeserializeOwned, Deserialize, Serialize};

pub fn config_dir() -> PathBuf {
    if let Ok(p) = std::env::var("COLLECTIVE_CONFIG_DIR") {
        return PathBuf::from(p);
    }
    dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("collective")
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
    pub use_replaygain: bool,
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
    pub light_theme: bool,
    pub ui_scale: f32,
    /// Separate every loaded track into stems in the background.
    pub stems_enabled: bool,
    /// ONNX separation model. `None` uses `STEMS_MODEL`, else the low-quality placeholder.
    pub stems_model: Option<PathBuf>,
    /// Disk cache for separated stems (MB); 0 disables it.
    pub stems_cache_mb: u64,
    /// External separator command (e.g. the Demucs add-on), tried before the model; empty = off.
    /// See `stems::external` for the protocol.
    pub stems_external: String,
    /// Where prepared stems (external separator, `--prepare-stems`) are kept; never cleaned up
    /// automatically. `None` = `stems-prepared` in the config folder.
    pub stems_prepared_dir: Option<PathBuf>,
    /// Mixing happens on an external (hardware) mixer: every deck goes to its own output pair,
    /// Collective's volume / trim / EQ / filter and the mixer section are bypassed and hidden.
    pub external_mixer: bool,
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
            default_loop_beats: 16.0,
            skip_beats: 16.0,
            load_at_first_cue: true,
            use_replaygain: false,
            osc_enabled: true,
            osc_port: 9090,
            osc_root: "/Collective/".into(),
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
            light_theme: false,
            ui_scale: 1.0,
            stems_enabled: true,
            stems_model: None,
            stems_cache_mb: 4096,
            stems_external: String::new(),
            stems_prepared_dir: None,
            external_mixer: false,
        }
    }
}

impl Settings {
    pub fn stems_prepared_dir(&self) -> PathBuf {
        self.stems_prepared_dir.clone().unwrap_or_else(|| config_dir().join("stems-prepared"))
    }

    /// The external separator command, if one is set.
    pub fn stems_external(&self) -> Option<String> {
        let c = self.stems_external.trim();
        (!c.is_empty()).then(|| c.to_string())
    }

    pub fn path() -> PathBuf {
        config_dir().join("settings.json")
    }

    pub fn load() -> Self {
        load_json(&Self::path())
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
