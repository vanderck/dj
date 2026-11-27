//! The session log: what happened while dj ran, to find out afterwards what went wrong in a mix.
//!
//! One file per session in the settings folder's `logs` (`session-YYYY-MM-DD-HHMMSS.jsonl`), one
//! JSON object per line, each with `t` (seconds since the session started) and `ev` (what):
//!
//! - `session`: when it started (UTC), dj's version and the settings that shape mixes;
//! - `load` / `loaded`: a track asked for on a deck (its path), then loaded (title, BPM, key…);
//! - `midi`: a controller message (its bytes, value, the addresses it's mapped to);
//! - `set`: a control changed (`by`: `you` on screen, `midi`, or `dj` mixing);
//! - `status`: dj's status messages (mixes starting, cancelled, done, …);
//! - `mix`: a mix starting (its plan);
//! - `tempo`, `clock`, `play`: a deck's BPM, the master or its BPM, a deck starting or stopping;
//! - `state`: every second, each loaded deck (position, BPM, volume, EQ, stems) and the clock;
//! - `mark`: you pressed Mark (where it went weird).
//!
//! The file is written as it goes (a crash keeps what came before).

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::time::Instant;

use serde_json::{json, Value};

pub struct MixLog {
    out: Option<BufWriter<File>>,
    pub path: Option<PathBuf>,
    started: Instant,
    /// Off (tests): nothing written.
    off: bool,
    /// Last value logged per control (a change is logged once it moves by 0.01).
    last_set: HashMap<String, String>,
    /// Who sets controls now: `you`, `midi` or `dj`.
    pub by: &'static str,
    /// When the last `state` line was written, and the last flush.
    last_state: f64,
    last_flush: f64,
}

impl Default for MixLog {
    fn default() -> Self {
        Self::new()
    }
}

impl MixLog {
    pub fn new() -> Self {
        Self {
            out: None,
            path: None,
            started: Instant::now(),
            off: cfg!(test),
            last_set: HashMap::new(),
            by: "you",
            last_state: f64::NEG_INFINITY,
            last_flush: 0.0,
        }
    }

    /// A log written to `path` (tests: the session log is off there).
    #[cfg(test)]
    pub fn at(path: &std::path::Path) -> Self {
        let mut l = Self::new();
        l.off = false;
        if let Some(d) = path.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        l.out = Some(BufWriter::new(File::create(path).expect("log file")));
        l.path = Some(path.to_path_buf());
        l
    }

    /// Seconds since the session started.
    pub fn now(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }

    fn open(&mut self) -> Option<&mut BufWriter<File>> {
        if self.off {
            return None;
        }
        if self.out.is_none() {
            let dir = crate::settings::config_dir().join("logs");
            let _ = std::fs::create_dir_all(&dir);
            let path = dir.join(format!("session-{}.jsonl", utc_stamp(std::time::SystemTime::now(), '-', "")));
            match File::create(&path) {
                Ok(f) => {
                    self.out = Some(BufWriter::new(f));
                    self.path = Some(path);
                    let header = json!({
                        "t": 0.0, "ev": "session",
                        "utc": utc_stamp(std::time::SystemTime::now(), '-', ":"),
                        "version": env!("CARGO_PKG_VERSION"),
                    });
                    if let Some(o) = self.out.as_mut() {
                        let _ = writeln!(o, "{header}");
                    }
                }
                Err(_) => {
                    self.off = true;
                    return None;
                }
            }
        }
        self.out.as_mut()
    }

    /// Write an event: `ev` and the fields in `fields` (an object).
    pub fn event(&mut self, ev: &str, fields: Value) {
        let t = (self.now() * 1000.0).round() / 1000.0;
        let mut obj = json!({ "t": t, "ev": ev });
        if let (Some(o), Value::Object(f)) = (obj.as_object_mut(), fields) {
            o.extend(f);
        }
        let flush = t - self.last_flush > 1.0 || ev == "mark";
        if let Some(out) = self.open() {
            let _ = writeln!(out, "{obj}");
            if flush {
                let _ = out.flush();
            }
        }
        if flush {
            self.last_flush = t;
        }
    }

    /// A control set to `value` (logged when it changed: floats to 0.01).
    pub fn set(&mut self, addr: &str, value: &str) {
        if self.off {
            return;
        }
        if self.last_set.get(addr).is_some_and(|v| v == value) {
            return;
        }
        self.last_set.insert(addr.to_string(), value.to_string());
        let by = self.by;
        self.event("set", json!({ "addr": addr, "value": value, "by": by }));
    }

    /// Whether `key` now reads `value` and didn't last time (for `tempo`, `clock`, `play`).
    pub fn changed(&mut self, key: &str, value: String) -> bool {
        if self.off {
            return false;
        }
        let k = format!("\u{0}{key}");
        if self.last_set.get(&k) == Some(&value) {
            return false;
        }
        self.last_set.insert(k, value);
        true
    }

    /// Whether a `state` line is due (every second).
    pub fn state_due(&mut self) -> bool {
        if self.off {
            return false;
        }
        let t = self.now();
        if t - self.last_state >= 1.0 {
            self.last_state = t;
            true
        } else {
            false
        }
    }

    pub fn flush(&mut self) {
        if let Some(o) = self.out.as_mut() {
            let _ = o.flush();
        }
    }

    pub fn enabled(&self) -> bool {
        !self.off
    }
}

/// UTC date and time of `t` as `2026-11-27{sep}153000` (with `tsep` between hours, minutes and
/// seconds).
fn utc_stamp(t: std::time::SystemTime, sep: char, tsep: &str) -> String {
    let secs = t.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + (m <= 2) as i64;
    format!("{y:04}-{m:02}-{d:02}{sep}{:02}{tsep}{:02}{tsep}{:02}", rem / 3600, rem / 60 % 60, rem % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_stamps() {
        let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_795_793_400);
        assert_eq!(utc_stamp(t, ' ', ":"), "2026-11-27 15:30:00");
        assert_eq!(utc_stamp(std::time::UNIX_EPOCH, '-', ""), "1970-01-01-000000");
    }
}
