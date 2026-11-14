use std::sync::Arc;

use nih_plug::prelude::*;
use nih_plug_egui::EguiState;
use stems_core::{FocusMode, FocusSettings, Settings, Stem, StemSettings};

#[derive(Enum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusModeParam {
    Off,
    /// Keep the selection on the main output, the rest goes to the Focus output.
    Isolate,
    /// Remove the selection from the main output and send it to the Focus output.
    Extract,
}

impl From<FocusModeParam> for FocusMode {
    fn from(m: FocusModeParam) -> Self {
        match m {
            FocusModeParam::Off => FocusMode::Off,
            FocusModeParam::Isolate => FocusMode::Isolate,
            FocusModeParam::Extract => FocusMode::Extract,
        }
    }
}

#[derive(Params)]
pub struct StemParams {
    #[id = "gain"]
    pub gain: FloatParam,
    #[id = "mute"]
    pub mute: BoolParam,
    #[id = "solo"]
    pub solo: BoolParam,
    #[id = "iso"]
    pub isolation: FloatParam,
}

impl StemParams {
    fn new(stem: Stem) -> Self {
        let name = stem.name();
        Self {
            gain: gain_param(format!("{name} Level"), -60.0, 12.0),
            mute: BoolParam::new(format!("{name} Mute"), false),
            solo: BoolParam::new(format!("{name} Solo"), false),
            isolation: FloatParam::new(
                format!("{name} Isolation"),
                0.0,
                FloatRange::Linear { min: 0.0, max: 1.0 },
            )
            .with_unit("%")
            .with_value_to_string(formatters::v2s_f32_percentage(0))
            .with_string_to_value(formatters::s2v_f32_percentage()),
        }
    }

    fn settings(&self) -> StemSettings {
        StemSettings {
            gain: self.gain.value(),
            mute: self.mute.value(),
            solo: self.solo.value(),
            isolation: self.isolation.value(),
        }
    }
}

#[derive(Params)]
pub struct FocusParams {
    #[id = "mode"]
    pub mode: EnumParam<FocusModeParam>,
    #[id = "low"]
    pub low_hz: FloatParam,
    #[id = "high"]
    pub high_hz: FloatParam,
    #[id = "pan"]
    pub pan_center: FloatParam,
    #[id = "width"]
    pub pan_width: FloatParam,
}

impl Default for FocusParams {
    fn default() -> Self {
        let freq = |name: &str, default: f32| {
            FloatParam::new(
                name,
                default,
                FloatRange::Skewed {
                    min: 20.0,
                    max: 20_000.0,
                    factor: FloatRange::skew_factor(-2.0),
                },
            )
            .with_value_to_string(formatters::v2s_f32_hz_then_khz(1))
            .with_string_to_value(formatters::s2v_f32_hz_then_khz())
        };
        Self {
            mode: EnumParam::new("Focus Mode", FocusModeParam::Off),
            low_hz: freq("Focus Low", 20.0),
            high_hz: freq("Focus High", 20_000.0),
            pan_center: FloatParam::new(
                "Focus Pan",
                0.0,
                FloatRange::Linear {
                    min: -1.0,
                    max: 1.0,
                },
            )
            .with_value_to_string(formatters::v2s_f32_panning())
            .with_string_to_value(formatters::s2v_f32_panning()),
            pan_width: FloatParam::new(
                "Focus Width",
                2.0,
                FloatRange::Linear { min: 0.0, max: 2.0 },
            )
            .with_value_to_string(Arc::new(|v| format!("{:.0}%", v * 50.0))),
        }
    }
}

#[derive(Params)]
pub struct StemsParams {
    #[persist = "editor-state"]
    pub editor_state: Arc<EguiState>,

    #[nested(id_prefix = "drums", group = "Drums")]
    pub drums: StemParams,
    #[nested(id_prefix = "bass", group = "Bass")]
    pub bass: StemParams,
    #[nested(id_prefix = "vocals", group = "Vocals")]
    pub vocals: StemParams,
    #[nested(id_prefix = "other", group = "Other")]
    pub other: StemParams,

    #[nested(id_prefix = "focus", group = "Focus EQ")]
    pub focus: FocusParams,

    #[id = "output"]
    pub output_gain: FloatParam,
}

impl Default for StemsParams {
    fn default() -> Self {
        Self {
            editor_state: EguiState::from_size(860, 520),
            drums: StemParams::new(Stem::Drums),
            bass: StemParams::new(Stem::Bass),
            vocals: StemParams::new(Stem::Vocals),
            other: StemParams::new(Stem::Other),
            focus: FocusParams::default(),
            output_gain: gain_param("Output".to_string(), -24.0, 12.0),
        }
    }
}

impl StemsParams {
    pub fn stem(&self, stem: Stem) -> &StemParams {
        match stem {
            Stem::Drums => &self.drums,
            Stem::Bass => &self.bass,
            Stem::Vocals => &self.vocals,
            Stem::Other => &self.other,
        }
    }

    /// Snapshot of all parameters for the DSP engine.
    pub fn to_settings(&self) -> Settings {
        Settings {
            stems: Stem::ALL.map(|s| self.stem(s).settings()),
            focus: FocusSettings {
                mode: self.focus.mode.value().into(),
                low_hz: self.focus.low_hz.value(),
                high_hz: self.focus.high_hz.value(),
                pan_center: self.focus.pan_center.value(),
                pan_width: self.focus.pan_width.value(),
            },
            output_gain: self.output_gain.value(),
        }
    }
}

fn gain_param(name: String, min_db: f32, max_db: f32) -> FloatParam {
    FloatParam::new(
        name,
        util::db_to_gain(0.0),
        FloatRange::Skewed {
            min: util::db_to_gain(min_db),
            max: util::db_to_gain(max_db),
            factor: FloatRange::gain_skew_factor(min_db, max_db),
        },
    )
    .with_unit(" dB")
    .with_value_to_string(formatters::v2s_f32_gain_to_db(1))
    .with_string_to_value(formatters::s2v_f32_gain_to_db())
}
