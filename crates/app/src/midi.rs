//! MIDI input (all or selected ports), output for LEDs, and MIDI clock out.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::{unbounded, Receiver};
use midir::{MidiInput, MidiInputConnection, MidiOutput, MidiOutputConnection};

use crate::settings::Settings;

struct Clock {
    bpm_bits: AtomicU64,
    running: AtomicBool,
    realign: AtomicBool,
}

pub struct Midi {
    _inputs: Vec<MidiInputConnection<()>>,
    rx: Receiver<Vec<u8>>,
    output: Option<Arc<Mutex<MidiOutputConnection>>>,
    clock: Arc<Clock>,
    pub input_names: Vec<String>,
    pub output_name: Option<String>,
    pub activity: Instant,
}

pub fn list_ports() -> (Vec<String>, Vec<String>) {
    let ins = MidiInput::new("dj-list")
        .map(|m| m.ports().iter().filter_map(|p| m.port_name(p).ok()).collect())
        .unwrap_or_default();
    let outs = MidiOutput::new("dj-list")
        .map(|m| m.ports().iter().filter_map(|p| m.port_name(p).ok()).collect())
        .unwrap_or_default();
    (ins, outs)
}

/// The MIDI ports, for the settings: listed in the background every couple of seconds (asking
/// CoreMIDI opens a client, which is slow: doing it every frame stalled the screen).
pub fn ports_cached() -> (Vec<String>, Vec<String>) {
    struct Cache {
        ports: (Vec<String>, Vec<String>),
        at: Option<Instant>,
        busy: bool,
    }
    static CACHE: Mutex<Cache> = Mutex::new(Cache {
        ports: (Vec::new(), Vec::new()),
        at: None,
        busy: false,
    });
    let mut c = CACHE.lock().unwrap_or_else(|p| p.into_inner());
    if !c.busy && c.at.is_none_or(|t| t.elapsed() > Duration::from_secs(2)) {
        c.busy = true;
        std::thread::spawn(|| {
            let ports = list_ports();
            let mut c = CACHE.lock().unwrap_or_else(|p| p.into_inner());
            c.ports = ports;
            c.at = Some(Instant::now());
            c.busy = false;
        });
    }
    c.ports.clone()
}

impl Midi {
    pub fn new(settings: &Settings) -> Self {
        let (tx, rx) = unbounded();
        let mut inputs = Vec::new();
        let mut input_names = Vec::new();
        if let Ok(probe) = MidiInput::new("dj") {
            for port in probe.ports() {
                let Ok(name) = probe.port_name(&port) else { continue };
                // Our own ports (ALSA lists them as "dj:dj-out …"). Not "contains": controllers
                // such as "DJControl …" must stay.
                if name.to_lowercase().starts_with("dj:") {
                    continue;
                }
                if !settings.midi_inputs.is_empty() && !settings.midi_inputs.iter().any(|n| n == &name) {
                    continue;
                }
                let Ok(mut mi) = MidiInput::new("dj") else { continue };
                mi.ignore(midir::Ignore::All);
                let tx = tx.clone();
                if let Ok(conn) = mi.connect(&port, "dj-in", move |_, msg, _| {
                    let _ = tx.send(msg.to_vec());
                }, ()) {
                    inputs.push(conn);
                    input_names.push(name);
                }
            }
        }
        let mut output = None;
        let mut output_name = None;
        if let (Some(want), Ok(mo)) = (&settings.midi_output, MidiOutput::new("dj")) {
            if let Some(port) = mo.ports().into_iter().find(|p| mo.port_name(p).ok().as_deref() == Some(want.as_str())) {
                if let Ok(conn) = mo.connect(&port, "dj-out") {
                    output = Some(Arc::new(Mutex::new(conn)));
                    output_name = Some(want.clone());
                }
            }
        }
        let clock = Arc::new(Clock {
            bpm_bits: AtomicU64::new(128f64.to_bits()),
            running: AtomicBool::new(settings.midi_clock_out),
            realign: AtomicBool::new(false),
        });
        if let Some(out) = &output {
            spawn_clock(out.clone(), clock.clone());
        }
        Midi { _inputs: inputs, rx, output, clock, input_names, output_name, activity: Instant::now() - Duration::from_secs(10) }
    }

    pub fn poll(&mut self) -> Vec<Vec<u8>> {
        let v: Vec<Vec<u8>> = self.rx.try_iter().collect();
        if !v.is_empty() {
            self.activity = Instant::now();
        }
        v
    }

    /// Whether a hardware controller is connected (virtual ports such as the macOS IAC driver,
    /// "Midi Through" or network sessions do not count).
    pub fn has_controller(&self) -> bool {
        self.input_names.iter().any(|n| !is_virtual_port(n))
    }

    pub fn has_output(&self) -> bool {
        self.output.is_some()
    }

    pub fn send(&self, msg: &[u8]) {
        if let Some(o) = &self.output {
            if let Ok(mut o) = o.lock() {
                let _ = o.send(msg);
            }
        }
    }

    pub fn set_clock(&self, bpm: f64) {
        self.clock.bpm_bits.store(bpm.to_bits(), Ordering::Relaxed);
    }

    pub fn set_clock_running(&self, on: bool) {
        self.clock.running.store(on, Ordering::Relaxed);
    }

    /// Called on every master beat so the 24 ppqn stream stays phase-locked.
    pub fn align_beat(&self) {
        self.clock.realign.store(true, Ordering::Relaxed);
    }
}

fn spawn_clock(out: Arc<Mutex<MidiOutputConnection>>, clock: Arc<Clock>) {
    std::thread::Builder::new()
        .name("midi-clock".into())
        .spawn(move || {
            let mut was_running = false;
            let mut next = Instant::now();
            let mut tick_in_beat = 0u32;
            loop {
                let running = clock.running.load(Ordering::Relaxed);
                if running != was_running {
                    if let Ok(mut o) = out.lock() {
                        let _ = o.send(if running { &[0xfa] } else { &[0xfc] });
                    }
                    was_running = running;
                    next = Instant::now();
                    tick_in_beat = 0;
                }
                if !running {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                let bpm = f64::from_bits(clock.bpm_bits.load(Ordering::Relaxed)).clamp(20.0, 999.0);
                let period = Duration::from_secs_f64(60.0 / bpm / 24.0);
                if clock.realign.swap(false, Ordering::Relaxed) {
                    // Snap to the beat: emit any missing ticks of the old beat now.
                    if tick_in_beat > 12 {
                        while tick_in_beat < 24 {
                            if let Ok(mut o) = out.lock() {
                                let _ = o.send(&[0xf8]);
                            }
                            tick_in_beat += 1;
                        }
                    }
                    tick_in_beat %= 24;
                    next = Instant::now();
                }
                let now = Instant::now();
                if now >= next {
                    if let Ok(mut o) = out.lock() {
                        let _ = o.send(&[0xf8]);
                    }
                    tick_in_beat = (tick_in_beat + 1) % 24;
                    next += period;
                    if now > next + period * 4 {
                        next = now + period;
                    }
                } else {
                    let wait = next - now;
                    if wait > Duration::from_millis(2) {
                        std::thread::sleep(wait - Duration::from_millis(1));
                    } else {
                        std::thread::yield_now();
                    }
                }
            }
        })
        .ok();
}

pub fn is_virtual_port(name: &str) -> bool {
    let n = name.to_lowercase();
    ["iac", "through", "network", "session", "virtual", "loopmidi", "rtpmidi"].iter().any(|v| n.contains(v))
}

#[cfg(test)]
mod port_tests {
    #[test]
    fn virtual_ports_are_not_controllers() {
        use super::is_virtual_port;
        assert!(is_virtual_port("IAC Driver Bus 1"));
        assert!(is_virtual_port("Midi Through Port-0"));
        assert!(is_virtual_port("Network Session 1"));
        assert!(!is_virtual_port("XONE:K2"));
        assert!(!is_virtual_port("Traktor Kontrol S4 MK3"));
    }
}
