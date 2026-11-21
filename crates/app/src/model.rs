//! The session: every user-facing setting that the engine mirrors.
//! Serialized on exit and restored on launch.

use std::path::PathBuf;

use dj_engine::deck::{Quantize, TempoRange};
use dj_engine::engine::DeckMode;
use dj_engine::fx::{InsertPos, MacroTarget, MACRO_TARGETS, MAX_SLOTS, NUM_MACROS};
use dj_engine::mixer::ChannelParams;
use dj_engine::sampler::{LaneParams, Pattern, Step, LANES, PATTERNS, STEPS};
use serde::{Deserialize, Serialize};

use crate::settings::{config_dir, load_json, save_json};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FxSlotModel {
    /// "native:<id>" or "vst3:<HEX CID>".
    pub effect_id: String,
    pub name: String,
    pub vendor: String,
    pub pos: InsertPos,
    pub on: bool,
    pub mix: f32,
    /// Normalized parameter values (mirror of the running effect).
    pub params: Vec<f32>,
    /// VST3 component/controller state, base64.
    #[serde(default)]
    pub vst3_state: Option<(String, String)>,
    /// The effect couldn't be loaded; settings are kept until it's replaced.
    #[serde(skip)]
    pub missing: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MacroModel {
    pub value: f32,
    pub targets: Vec<MacroTarget>,
}

impl MacroModel {
    pub fn engine_targets(&self) -> [Option<MacroTarget>; MACRO_TARGETS] {
        let mut t = [None; MACRO_TARGETS];
        for (i, x) in self.targets.iter().take(MACRO_TARGETS).enumerate() {
            t[i] = Some(*x);
        }
        t
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[derive(Default)]
pub struct FxChainModel {
    pub name: String,
    pub slots: [Option<FxSlotModel>; MAX_SLOTS],
    pub bypass_all: bool,
    pub macros: [MacroModel; NUM_MACROS],
}


impl FxChainModel {
    pub fn presets_dir() -> PathBuf {
        config_dir().join("fx-presets")
    }

    pub fn save_preset(&self, name: &str) -> anyhow::Result<PathBuf> {
        let safe: String = name.chars().map(|c| if c.is_alphanumeric() || " -_".contains(c) { c } else { '_' }).collect();
        let p = Self::presets_dir().join(format!("{safe}.json"));
        let mut copy = self.clone();
        copy.name = name.to_string();
        save_json(&p, &copy)?;
        Ok(p)
    }

    pub fn list_presets() -> Vec<(String, PathBuf)> {
        let mut v: Vec<(String, PathBuf)> = std::fs::read_dir(Self::presets_dir())
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().map(|e| e == "json").unwrap_or(false))
            .map(|p| (p.file_stem().unwrap().to_string_lossy().into_owned(), p))
            .collect();
        v.sort();
        v
    }

    pub fn load_preset(path: &std::path::Path) -> Option<Self> {
        let c: FxChainModel = load_json(path);
        (c != FxChainModel::default()).then_some(c)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
pub struct ChannelModel {
    pub params: ChannelParams,
    pub fader_start: bool,
    pub fx: FxChainModel,
    /// UI: show EQ (true) or filter (false) knobs.
    pub show_eq: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[derive(Default)]
pub struct LaneModel {
    pub params: LaneParams,
    pub sample: Option<PathBuf>,
}


#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SamplerModel {
    pub name: String,
    pub lanes: [LaneModel; LANES],
    pub patterns: Vec<Pattern>,
    pub pattern: usize,
    pub swing: f32,
    pub global_pitch: f32,
    pub speed: f64,
    pub selected_lane: usize,
}

impl Default for SamplerModel {
    fn default() -> Self {
        Self {
            name: "Sampler".into(),
            lanes: Default::default(),
            patterns: vec![[[Step::default(); STEPS]; LANES]; PATTERNS],
            pattern: 0,
            swing: 0.0,
            global_pitch: 0.0,
            speed: 1.0,
            selected_lane: 0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeckModel {
    pub mode: DeckMode,
    pub track: Option<PathBuf>,
    pub keylock: bool,
    pub tempo_range: TempoRange,
    pub quantize: Quantize,
    pub slip: bool,
    pub momentary_cues: bool,
    pub sampler: SamplerModel,
    pub slip_dotted: bool,
    pub slip_triplet: bool,
    pub zoom: f32,
}

impl Default for DeckModel {
    fn default() -> Self {
        Self {
            mode: DeckMode::Track,
            track: None,
            keylock: true,
            tempo_range: TempoRange::Pct10,
            quantize: Quantize::UseGlobal,
            slip: false,
            momentary_cues: false,
            sampler: SamplerModel::default(),
            slip_dotted: false,
            slip_triplet: false,
            zoom: 1.0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MasterModel {
    /// 0..1 knob; 0.85 ≈ 0 dB.
    pub volume: f32,
    pub limiter: bool,
    pub cue_volume: f32,
    pub cue_mix: f32,
}

impl Default for MasterModel {
    fn default() -> Self {
        Self {
            volume: 0.85,
            limiter: true,
            cue_volume: 0.75,
            cue_mix: 0.0,
        }
    }
}

impl MasterModel {
    /// Knob → linear gain: −60 dB … +6 dB with 0 dB at 0.85 (~10 dB above at full).
    pub fn knob_gain(v: f32) -> f32 {
        if v <= 0.0 {
            return 0.0;
        }
        let db = if v < 0.85 { -60.0 + 60.0 * (v / 0.85) } else { (v - 0.85) / 0.15 * 6.0 };
        dj_engine::dsp::db_to_gain(db)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Session {
    pub channels: [ChannelModel; 4],
    pub decks: [DeckModel; 4],
    pub master: MasterModel,
    pub bpm: f64,
    /// Index into the quantize list (see `QUANTIZE_VALUES`).
    pub quantize: usize,
    pub quantize_on: bool,
    pub wallclock: bool,
}

impl Default for Session {
    fn default() -> Self {
        Self {
            channels: Default::default(),
            decks: Default::default(),
            master: MasterModel::default(),
            bpm: 128.0,
            quantize: 3,
            quantize_on: true,
            wallclock: false,
        }
    }
}

/// Quantize values in beats: 2 bars, 1 bar, 1/2, 1/4, 1/8, 1/16, 1/32 bar.
pub const QUANTIZE_VALUES: [(f64, &str); 7] =
    [(8.0, "2"), (4.0, "1"), (2.0, "1/2"), (1.0, "1/4"), (0.5, "1/8"), (0.25, "1/16"), (0.125, "1/32")];

impl Session {
    pub fn path() -> PathBuf {
        config_dir().join("session.json")
    }

    pub fn load() -> Self {
        load_json(&Self::path())
    }

    pub fn save(&self) -> anyhow::Result<()> {
        save_json(&Self::path(), self)
    }

    pub fn global_quantize(&self) -> Option<f64> {
        self.quantize_on.then(|| QUANTIZE_VALUES[self.quantize.min(QUANTIZE_VALUES.len() - 1)].0)
    }
}
