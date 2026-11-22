//! Ready-made controller mappings.
//!
//! The Pioneer DJ OPUS-QUAD in PC control mode, from AlphaTheta's "OPUS-QUAD MIDI Message List"
//! (E1): decks 1–4 on MIDI channels 1–4 (deck select switches a side between 1/3 and 2/4), the
//! mixer and effects on channel 5, the browser on channel 1. Untested on the hardware so far.

use crate::settings::{MapMode, MappingProfile, MidiKind, MidiMapping, MidiSource};

fn map(kind: MidiKind, channel: u8, number: u8, address: String, mode: MapMode, feedback: bool) -> MidiMapping {
    MidiMapping { source: MidiSource { kind, channel, number }, address, mode, invert: false, feedback }
}

/// The OPUS-QUAD's controls mapped to dj (see the module docs). Not mapped: the crossfader (dj
/// has none), the beat effects and Sound Color FX buttons, key sync, tempo range, track search,
/// needle search (its 0 also means "finger lifted"), jog touch and the jog mode switch.
pub fn opus_quad() -> Vec<MidiMapping> {
    use MapMode::{Absolute, RelativeCentred, Toggle};
    use MidiKind::{Cc, Note};
    let mut m = Vec::new();
    for d in 0..4u8 {
        let n = d + 1;
        let deck = |s: &str| format!("Deck/{n}/{s}");
        let note = |number: u8, s: &str, mode: MapMode, fb: bool| map(Note, d, number, deck(s), mode, fb);
        m.push(note(0, "PlayPause", Absolute, true));
        m.push(note(1, "Cue", Absolute, true));
        m.push(note(31, "TempoSync", Absolute, true));
        m.push(note(30, "TempoMaster", Absolute, true));
        m.push(note(17, "PitchPreserve", Toggle, true)); // MASTER TEMPO: key lock
        m.push(note(19, "Tempo:0.5", Absolute, false)); // TEMPO RESET
        m.push(map(Note, d, 9, "Header/QuantizeOn".into(), Toggle, true)); // DECK QUANTIZE (dj's is global)
        m.push(note(6, "LoopIn", Absolute, true));
        m.push(note(7, "LoopOut", Absolute, true));
        m.push(note(67, "Loop", Absolute, false)); // BEAT LOOP press
        m.push(note(44, "SkipBarForward", Absolute, false)); // BEAT JUMP
        m.push(note(45, "SkipBarBack", Absolute, false));
        m.push(note(73, "SkipBeatForward", Absolute, false)); // +SHIFT: a beat
        m.push(note(74, "SkipBeatBack", Absolute, false));
        m.push(note(33, "Reverse", Absolute, true)); // SLIP REVERSE (while held)
        m.push(note(81, "LoadSelected", Absolute, false));
        // Hot cues A–H, and with SHIFT: delete that cue.
        for (i, (press, shift)) in [(8, 10), (11, 12), (13, 14), (15, 20), (21, 22), (23, 24), (25, 26), (27, 28)].into_iter().enumerate() {
            m.push(note(press, &format!("Cue{}", i + 1), Absolute, true));
            m.push(note(shift, &format!("DeleteCue:{}", i + 1), Absolute, false));
        }
        // Jog wheel (top and side), tempo fader (14-bit: CC 0 and its fine half, CC 32; "-" at
        // 0), the beat loop encoder (±1–30, two's complement).
        m.push(map(Cc, d, 34, deck("Jog"), RelativeCentred, false));
        m.push(map(Cc, d, 33, deck("Jog"), RelativeCentred, false));
        m.push(map(Cc, d, 0, deck("Tempo"), Absolute, false));
        m.push(map(Cc, d, 46, deck("LoopSizeStep"), MapMode::Relative, false));
    }
    // Mixer (channel 5): per channel CH 1–4.
    let mix = |number: u8, ch: usize, s: &str, mode: MapMode| map(Cc, 4, number, format!("Mixer/CH{ch}/{s}"), mode, false);
    for (ch, [fader, trim, hi, mid, low, color, cue]) in
        [(1, [17, 1, 2, 3, 4, 5, 70]), (2, [18, 6, 7, 8, 9, 10, 71]), (3, [19, 12, 14, 15, 21, 22, 72]), (4, [20, 80, 81, 92, 82, 83, 73])]
    {
        m.push(mix(fader, ch, "Volume", Absolute));
        m.push(mix(trim, ch, "Trim", Absolute));
        m.push(mix(hi, ch, "EQ/High", Absolute));
        m.push(mix(mid, ch, "EQ/Mid", Absolute));
        m.push(mix(low, ch, "EQ/Low", Absolute));
        m.push(mix(color, ch, "Filter1", Absolute)); // the COLOR knob: dj's filter
        m.push(mix(cue, ch, "Cue", Toggle)); // headphone cue
    }
    m.push(map(Cc, 4, 24, "Header/MasterVolume".into(), Absolute, false));
    m.push(map(Cc, 4, 26, "Header/CueVolume".into(), Absolute, false));
    m.push(map(Cc, 4, 27, "Header/CueMix".into(), Absolute, false));
    // Browser (channel 1): the rotary selector (±1–30, two's complement).
    m.push(map(Cc, 0, 79, "Browser/Browse".into(), MapMode::Relative, false));
    m
}

/// Add a preset to the profile: mappings on the same MIDI controls are replaced, the others kept.
/// Returns how many it added.
pub fn apply(profile: &mut MappingProfile, preset: Vec<MidiMapping>) -> usize {
    let sources: Vec<MidiSource> = preset.iter().map(|m| m.source).collect();
    profile.mappings.retain(|m| !sources.contains(&m.source));
    let n = preset.len();
    profile.mappings.extend(preset);
    n
}
