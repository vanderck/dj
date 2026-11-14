use crate::FocusSettings;

/// Width of the soft frequency edges, in octaves.
const FREQ_EDGE_OCTAVES: f32 = 1.0 / 6.0;
/// Width of the soft stereo-position edges, in pan units.
const PAN_EDGE: f32 = 0.1;

fn smoothstep(x: f32) -> f32 {
    let x = x.clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

/// Soft step that is 0.5 at `x == 0` and reaches 0/1 at `-edge`/`+edge`.
fn soft_step(x: f32, edge: f32) -> f32 {
    smoothstep(0.5 + x / (2.0 * edge))
}

/// How much of a time-frequency bin at `freq_hz` and stereo position `pan` (-1..=1) lies inside
/// the Focus EQ selection, in 0..=1. Independent of [`FocusSettings::mode`].
pub fn focus_weight(settings: &FocusSettings, freq_hz: f32, pan: f32) -> f32 {
    let f = freq_hz.max(1.0);
    let lo = settings.low_hz.max(1.0);
    let hi = settings.high_hz.max(lo);
    let band = soft_step((f / lo).log2(), FREQ_EDGE_OCTAVES)
        * soft_step((hi / f).log2(), FREQ_EDGE_OCTAVES);

    let pan_sel = if settings.pan_width >= 2.0 {
        1.0
    } else {
        let half = settings.pan_width.max(0.0) * 0.5;
        soft_step(half - (pan - settings.pan_center).abs(), PAN_EDGE)
    };

    band * pan_sel
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_band_and_pan() {
        let s = FocusSettings {
            low_hz: 200.0,
            high_hz: 2000.0,
            pan_center: 0.0,
            pan_width: 0.5,
            ..Default::default()
        };
        assert!(focus_weight(&s, 600.0, 0.0) > 0.99);
        assert!(focus_weight(&s, 50.0, 0.0) < 0.01);
        assert!(focus_weight(&s, 8000.0, 0.0) < 0.01);
        assert!(focus_weight(&s, 600.0, 0.9) < 0.01);
    }
}
