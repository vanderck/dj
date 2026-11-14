//! Musical keys as written by Mixed In Key (Camelot), plus musical and Open
//! Key notation, transposition and harmonic-mixing compatibility.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Key {
    /// Tonic pitch class, 0 = C.
    pub pitch: u8,
    pub minor: bool,
}

const NAMES: [&str; 12] = ["C", "Db", "D", "Eb", "E", "F", "F#", "G", "Ab", "A", "Bb", "B"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compat {
    Same,
    /// ±1 on the Camelot wheel, same mode.
    Adjacent,
    /// Relative major/minor (same number, other letter).
    Relative,
    /// +2 on the wheel ("energy boost").
    Boost,
    Diagonal,
    Clash,
}

impl Compat {
    pub fn is_good(self) -> bool {
        matches!(self, Compat::Same | Compat::Adjacent | Compat::Relative)
    }
}

impl Key {
    pub fn new(pitch: u8, minor: bool) -> Self {
        Self { pitch: pitch % 12, minor }
    }

    /// Camelot number 1..=12.
    pub fn camelot_number(self) -> u8 {
        let major_pc = if self.minor { (self.pitch + 3) % 12 } else { self.pitch };
        let n = (major_pc as u32 * 7 + 8) % 12;
        if n == 0 {
            12
        } else {
            n as u8
        }
    }

    pub fn camelot(self) -> String {
        format!("{}{}", self.camelot_number(), if self.minor { 'A' } else { 'B' })
    }

    pub fn open_key(self) -> String {
        let n = (self.camelot_number() as u32 + 4) % 12 + 1;
        format!("{}{}", n, if self.minor { 'm' } else { 'd' })
    }

    pub fn musical(self) -> String {
        format!("{}{}", NAMES[self.pitch as usize], if self.minor { "m" } else { "" })
    }

    pub fn from_camelot(n: u8, minor: bool) -> Option<Self> {
        if !(1..=12).contains(&n) {
            return None;
        }
        // Inverse of camelot_number: pc*7 ≡ n-8 (mod 12); 7⁻¹ ≡ 7 (mod 12).
        let major_pc = (((n as i32 - 8).rem_euclid(12)) * 7).rem_euclid(12) as u8;
        let pitch = if minor { (major_pc + 9) % 12 } else { major_pc };
        Some(Self::new(pitch, minor))
    }

    pub fn transpose(self, semitones: i32) -> Self {
        Self::new(((self.pitch as i32 + semitones).rem_euclid(12)) as u8, self.minor)
    }

    pub fn compat(self, other: Key) -> Compat {
        let a = self.camelot_number() as i32;
        let b = other.camelot_number() as i32;
        let d = (b - a).rem_euclid(12);
        let dist = d.min(12 - d);
        match (self.minor == other.minor, dist, d) {
            (true, 0, _) => Compat::Same,
            (true, 1, _) => Compat::Adjacent,
            (false, 0, _) => Compat::Relative,
            (true, 2, 2) => Compat::Boost,
            (false, 1, _) => Compat::Diagonal,
            _ => Compat::Clash,
        }
    }

    /// Parse the many ways a key ends up in a tag: "8A", "08A", "Am",
    /// "A minor", "G#m", "Abm", "C", "1d", "6m" (Open Key), "8A - Energy 6".
    pub fn parse(s: &str) -> Option<Self> {
        let t = s.trim();
        let t = t.split(|c: char| c == '/' || c == ',' || c == ' ' && t.contains('-')).next().unwrap_or(t).trim();
        if t.is_empty() {
            return None;
        }
        let lower = t.to_ascii_lowercase();
        // Camelot / Open Key: digits followed by a letter.
        let digits: String = lower.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !digits.is_empty() {
            let n: u8 = digits.parse().ok()?;
            let rest = lower[digits.len()..].trim();
            return match rest {
                "a" => Key::from_camelot(n, true),
                "b" => Key::from_camelot(n, false),
                "m" => Key::from_camelot(((n as i32 - 6).rem_euclid(12) + 1) as u8, true),
                "d" => Key::from_camelot(((n as i32 - 6).rem_euclid(12) + 1) as u8, false),
                _ => None,
            };
        }
        // Musical notation.
        let mut chars = t.chars();
        let letter = chars.next()?.to_ascii_uppercase();
        let base: i32 = match letter {
            'C' => 0,
            'D' => 2,
            'E' => 4,
            'F' => 5,
            'G' => 7,
            'A' => 9,
            'B' => 11,
            _ => return None,
        };
        let rest: String = chars.collect();
        let mut r = rest.trim_start();
        let mut acc = 0;
        if let Some(x) = r.strip_prefix('#').or_else(|| r.strip_prefix('♯')) {
            acc = 1;
            r = x;
        } else if let Some(x) = r.strip_prefix('b').or_else(|| r.strip_prefix('♭')) {
            // "b" could also start "bm"? Only if followed by m/min/maj/end.
            acc = -1;
            r = x;
        }
        let r = r.trim().to_ascii_lowercase();
        let minor = match r.as_str() {
            "" | "maj" | "major" | "dur" => false,
            "m" | "min" | "minor" | "moll" | "-" => true,
            _ => return None,
        };
        Some(Key::new((base + acc).rem_euclid(12) as u8, minor))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn camelot_wheel() {
        assert_eq!(Key::parse("Am").unwrap().camelot(), "8A");
        assert_eq!(Key::parse("C").unwrap().camelot(), "8B");
        assert_eq!(Key::parse("B").unwrap().camelot(), "1B");
        assert_eq!(Key::parse("Abm").unwrap().camelot(), "1A");
        assert_eq!(Key::parse("G#m").unwrap().camelot(), "1A");
        assert_eq!(Key::parse("C#m").unwrap().camelot(), "12A");
        assert_eq!(Key::parse("F#").unwrap().camelot(), "2B");
        for n in 1..=12 {
            for m in [true, false] {
                let k = Key::from_camelot(n, m).unwrap();
                assert_eq!(k.camelot_number(), n);
                assert_eq!(Key::parse(&k.camelot()), Some(k));
                assert_eq!(Key::parse(&k.open_key()), Some(k));
                assert_eq!(Key::parse(&k.musical()), Some(k));
            }
        }
    }

    #[test]
    fn mik_style_strings() {
        assert_eq!(Key::parse("08A").unwrap().musical(), "Am");
        assert_eq!(Key::parse("8A - Energy 6").unwrap().musical(), "Am");
        assert_eq!(Key::parse("A minor").unwrap().camelot(), "8A");
        assert_eq!(Key::parse("1d").unwrap().musical(), "C");
        assert_eq!(Key::parse("hello"), None);
    }

    #[test]
    fn transposition_moves_seven_steps_per_semitone() {
        let am = Key::parse("8A").unwrap();
        assert_eq!(am.transpose(1).camelot(), "3A"); // Bbm
        assert_eq!(am.transpose(2).camelot(), "10A"); // Bm
        assert_eq!(am.transpose(-1).camelot(), "1A"); // Abm
    }

    #[test]
    fn compatibility() {
        let k = |s| Key::parse(s).unwrap();
        assert_eq!(k("8A").compat(k("8A")), Compat::Same);
        assert_eq!(k("8A").compat(k("9A")), Compat::Adjacent);
        assert_eq!(k("8A").compat(k("7A")), Compat::Adjacent);
        assert_eq!(k("12A").compat(k("1A")), Compat::Adjacent);
        assert_eq!(k("8A").compat(k("8B")), Compat::Relative);
        assert_eq!(k("8A").compat(k("10A")), Compat::Boost);
        assert_eq!(k("8A").compat(k("3B")), Compat::Clash);
    }
}
