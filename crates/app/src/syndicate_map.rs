//! Import MIDI mappings exported by Syndicate (`.sdcmap`, "syndicate-midi-mapping" JSON).
//!
//! Each binding's `target` (e.g. `deck.1.cue.2`, `mixer.ch3.volume`) is translated to the matching
//! dj address (`Deck/1/Cue2`, `Mixer/CH3/Volume`); `kind` 0 is a CC, 1 a note; `channel`
//! and `number` are taken as raw MIDI values (channel 0..15).

use serde::Deserialize;

use crate::settings::{MapMode, MappingProfile, MidiKind, MidiMapping, MidiSource};

#[derive(Debug, Deserialize)]
struct File {
    format: String,
    #[serde(default)]
    version: u32,
    #[serde(default)]
    bindings: Vec<Binding>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Binding {
    target: String,
    #[serde(default)]
    device_name: String,
    kind: u32,
    channel: u8,
    number: u8,
    #[serde(default = "yes")]
    enabled: bool,
    #[serde(default)]
    inverted: bool,
    #[serde(default)]
    toggle: bool,
    #[serde(default = "midi")]
    protocol: String,
}

fn yes() -> bool {
    true
}

fn midi() -> String {
    "midi".into()
}

#[derive(Debug, Default, PartialEq)]
pub struct ImportReport {
    pub imported: usize,
    /// Existing mappings for the same MIDI control that were replaced.
    pub replaced: usize,
    /// Bindings that could not be translated: (target, reason).
    pub skipped: Vec<(String, String)>,
    /// MIDI device names used by the file.
    pub devices: Vec<String>,
    pub format_version: u32,
}

/// Translate a Syndicate target to a dj address.
pub fn address_for(target: &str) -> Option<String> {
    let p: Vec<&str> = target.split('.').collect();
    let n = |s: &str| s.parse::<u8>().ok().filter(|n| (1..=4).contains(n));
    Some(match p.as_slice() {
        ["deck", d, "cue", k] => {
            let k = k.parse::<u8>().ok().filter(|k| (1..=8).contains(k))?;
            format!("Deck/{}/Cue{k}", n(d)?)
        }
        ["deck", d, rest] => {
            let name = match *rest {
                "loop" => "Loop",
                "loop_in" => "LoopIn",
                "loop_out" => "LoopOut",
                "loop_exit" => "LoopExit",
                "play" | "play_pause" | "playpause" => "PlayPause",
                "cue" => "Cue",
                "sync" | "tempo_sync" => "TempoSync",
                "master" | "tempo_master" => "TempoMaster",
                "slip" => "Slip",
                "reverse" => "Reverse",
                "keylock" | "key_lock" | "pitch_preserve" => "PitchPreserve",
                "eject" => "Eject",
                _ => return None,
            };
            format!("Deck/{}/{name}", n(d)?)
        }
        ["mixer", ch, rest @ ..] => {
            let c = n(ch.strip_prefix("ch")?)?;
            let name = match rest {
                ["volume"] => "Volume",
                ["trim"] | ["gain"] => "Trim",
                ["cue"] => "Cue",
                ["mute"] => "Mute",
                ["solo"] => "Solo",
                ["filter"] => "Filter1",
                ["eq", "high"] | ["eq", "hi"] => "EQ/High",
                ["eq", "mid"] => "EQ/Mid",
                ["eq", "low"] => "EQ/Low",
                _ => return None,
            };
            format!("Mixer/CH{c}/{name}")
        }
        _ => return None,
    })
}

/// Merge the bindings of a `.sdcmap` file into `profile`.
pub fn import(json: &str, profile: &mut MappingProfile) -> Result<ImportReport, String> {
    let file: File = serde_json::from_str(json).map_err(|e| format!("not a Syndicate mapping file: {e}"))?;
    if file.format != "syndicate-midi-mapping" {
        return Err(format!("unsupported format \"{}\" (expected syndicate-midi-mapping)", file.format));
    }
    let mut report = ImportReport { format_version: file.version, ..Default::default() };
    for b in file.bindings {
        if !b.device_name.is_empty() && !report.devices.contains(&b.device_name) {
            report.devices.push(b.device_name.clone());
        }
        let skip = |r: &mut ImportReport, why: &str| r.skipped.push((b.target.clone(), why.to_string()));
        if !b.enabled {
            skip(&mut report, "disabled in Syndicate");
            continue;
        }
        if b.protocol != "midi" {
            skip(&mut report, "not a MIDI binding");
            continue;
        }
        let kind = match b.kind {
            0 => MidiKind::Cc,
            1 => MidiKind::Note,
            other => {
                skip(&mut report, &format!("unknown MIDI kind {other}"));
                continue;
            }
        };
        if b.channel > 15 || b.number > 127 {
            skip(&mut report, "MIDI channel or number out of range");
            continue;
        }
        let Some(address) = address_for(&b.target) else {
            skip(&mut report, "no matching dj control");
            continue;
        };
        let source = MidiSource { kind, channel: b.channel, number: b.number };
        let before = profile.mappings.len();
        profile.mappings.retain(|m| m.source != source);
        report.replaced += before - profile.mappings.len();
        profile.mappings.push(MidiMapping {
            source,
            address,
            mode: if b.toggle { MapMode::Toggle } else { MapMode::Absolute },
            invert: b.inverted,
            feedback: true,
        });
        report.imported += 1;
    }
    Ok(report)
}

impl ImportReport {
    pub fn summary(&self) -> String {
        let mut s = format!("Imported {} MIDI mapping(s)", self.imported);
        if self.replaced > 0 {
            s += &format!(", replacing {}", self.replaced);
        }
        if !self.devices.is_empty() {
            s += &format!(" for {}", self.devices.join(", "));
        }
        if !self.skipped.is_empty() {
            let list: Vec<String> = self.skipped.iter().map(|(t, why)| format!("{t} ({why})")).collect();
            s += &format!(". Skipped {}: {}", self.skipped.len(), list.join(", "));
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding(target: &str, kind: u32, ch: u8, num: u8) -> String {
        format!(
            r#"{{"target":"{target}","deviceId":"1","deviceName":"XONE:K2","kind":{kind},"channel":{ch},"number":{num},
               "enabled":true,"inverted":false,"toggle":false,"minimum":0.0,"maximum":1.0,"protocol":"midi","keyCode":0,"keyModifiers":0}}"#
        )
    }

    fn file(bindings: &[String]) -> String {
        format!(r#"{{"format":"syndicate-midi-mapping","version":4,"bindings":[{}]}}"#, bindings.join(","))
    }

    #[test]
    fn translates_targets() {
        assert_eq!(address_for("deck.1.cue.2").as_deref(), Some("Deck/1/Cue2"));
        assert_eq!(address_for("deck.4.loop_out").as_deref(), Some("Deck/4/LoopOut"));
        assert_eq!(address_for("deck.3.loop").as_deref(), Some("Deck/3/Loop"));
        assert_eq!(address_for("mixer.ch2.volume").as_deref(), Some("Mixer/CH2/Volume"));
        assert_eq!(address_for("mixer.ch4.trim").as_deref(), Some("Mixer/CH4/Trim"));
        assert_eq!(address_for("mixer.ch1.eq.low").as_deref(), Some("Mixer/CH1/EQ/Low"));
        assert_eq!(address_for("deck.5.cue.1"), None);
        assert_eq!(address_for("deck.1.cue.9"), None);
        assert_eq!(address_for("fx.1.wet"), None);
    }

    #[test]
    fn imports_and_replaces_by_midi_control() {
        let mut profile = MappingProfile::default();
        let src = MidiSource { kind: MidiKind::Cc, channel: 3, number: 16 };
        profile.mappings.push(MidiMapping { source: src, address: "Deck/1/Tempo".into(), mode: MapMode::Absolute, invert: false, feedback: true });
        let json = file(&[
            binding("deck.1.cue.1", 1, 3, 24),
            binding("mixer.ch1.volume", 0, 3, 16),
            binding("browser.scroll", 0, 3, 0),
        ]);
        let r = import(&json, &mut profile).unwrap();
        assert_eq!((r.imported, r.replaced, r.skipped.len()), (2, 1, 1));
        assert_eq!(r.devices, vec!["XONE:K2"]);
        let vol = profile.mappings.iter().find(|m| m.source == src).unwrap();
        assert_eq!(vol.address, "Mixer/CH1/Volume");
        let cue = profile.mappings.iter().find(|m| m.address == "Deck/1/Cue1").unwrap();
        assert_eq!(cue.source, MidiSource { kind: MidiKind::Note, channel: 3, number: 24 });
        assert!(r.summary().contains("browser.scroll"));
    }

    #[test]
    fn rejects_other_files() {
        assert!(import("{}", &mut MappingProfile::default()).is_err());
        assert!(import(r#"{"format":"something-else","bindings":[]}"#, &mut MappingProfile::default()).is_err());
    }
}
