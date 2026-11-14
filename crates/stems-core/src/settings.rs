use crate::NUM_STEMS;

/// Per-stem controls.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StemSettings {
    /// Linear gain.
    pub gain: f32,
    pub mute: bool,
    pub solo: bool,
    /// 0 = transparent (all stems at unity sum back to the input exactly),
    /// 1 = maximally strict separation (less bleed, more artefacts).
    pub isolation: f32,
}

impl Default for StemSettings {
    fn default() -> Self {
        Self {
            gain: 1.0,
            mute: false,
            solo: false,
            isolation: 0.0,
        }
    }
}

/// What the Focus EQ does with the selected time-frequency/stereo region.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FocusMode {
    /// Focus EQ bypassed; the auxiliary output is silent.
    #[default]
    Off,
    /// Keep only the selected region on the main output; send the rest to the aux output.
    Isolate,
    /// Remove the selected region from the main output; send it to the aux output.
    Extract,
}

/// Focus EQ: selects a frequency range and a stereo-position range.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FocusSettings {
    pub mode: FocusMode,
    pub low_hz: f32,
    pub high_hz: f32,
    /// Centre of the selected stereo region, -1 (left) ..= 1 (right).
    pub pan_center: f32,
    /// Width of the selected stereo region, 0 ..= 2 (2 = everything).
    pub pan_width: f32,
}

impl Default for FocusSettings {
    fn default() -> Self {
        Self {
            mode: FocusMode::Off,
            low_hz: 20.0,
            high_hz: 20_000.0,
            pan_center: 0.0,
            pan_width: 2.0,
        }
    }
}

/// Everything the engine needs from the outside world, snapshotted once per host block.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Settings {
    pub stems: [StemSettings; NUM_STEMS],
    pub focus: FocusSettings,
    /// Linear gain applied to both outputs.
    pub output_gain: f32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            stems: [StemSettings::default(); NUM_STEMS],
            focus: FocusSettings::default(),
            output_gain: 1.0,
        }
    }
}

impl Settings {
    /// Effective linear gain per stem after applying mute and solo.
    pub fn effective_gains(&self) -> [f32; NUM_STEMS] {
        let any_solo = self.stems.iter().any(|s| s.solo);
        let mut out = [0.0; NUM_STEMS];
        for (g, s) in out.iter_mut().zip(&self.stems) {
            let audible = !s.mute && (!any_solo || s.solo);
            *g = if audible { s.gain } else { 0.0 };
        }
        out
    }

    /// Exponent applied to each stem's mask. 1.0 at isolation 0.
    pub fn isolation_exponents(&self) -> [f32; NUM_STEMS] {
        self.stems
            .map(|s| 1.0 + MAX_ISOLATION_EXTRA_EXPONENT * s.isolation.clamp(0.0, 1.0))
    }
}

/// At isolation = 1 masks are raised to the power `1 + this`.
const MAX_ISOLATION_EXTRA_EXPONENT: f32 = 4.0;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn solo_overrides_others_and_mute_wins() {
        let mut s = Settings::default();
        s.stems[1].solo = true;
        s.stems[2].solo = true;
        s.stems[2].mute = true;
        assert_eq!(s.effective_gains(), [0.0, 1.0, 0.0, 0.0]);
    }
}
