//! Application core: owns the session model, talks to the engine, and
//! routes every control surface (GUI, MIDI, keyboard, OSC) through one
//! address-based API that mirrors the original application's OSC API.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use dj_engine::deck::{DeckEvent, Quantize, TempoRange};
use dj_engine::engine::{ClockOwner, DeckCmd, DeckMode, SamplerCmd, NUM_DECKS};
use dj_engine::fx::{InsertPos, MacroTarget, MACRO_TARGETS, MAX_SLOTS, NUM_MACROS};
use dj_engine::mixer::{ChannelParams, ExtStages, Output};
use dj_engine::sampler::{LaneParams, Step, LANES, STEPS};
use dj_engine::track::{Cue, CueKind, NUM_HOT_CUES};
use dj_engine::{Command, Controller, Event, Garbage, Snapshot};
use dj_library::scan::Entry;
use dj_library::{GridSource, Key, LoadedTrack, Tags, TrackOverrides, Waveform};
use crossbeam_channel::{unbounded, Receiver, Sender};
use rosc::OscType;

#[path = "automix.rs"]
pub mod automix;
#[cfg(test)]
#[path = "mixlab.rs"]
mod mixlab;

use crate::fxcat::{self, CatalogEntry, FxLive, ScanCache};
use crate::model::{FxChainModel, FxSlotModel, MacroModel, Session, QUANTIZE_VALUES};
use crate::settings::{KeyNotation, MapMode, MappingProfile, MidiKind, MidiMapping, MidiSource, Settings, TrackDb};

pub const SLIP_LOOPS: [(&str, f64); 8] = [
    ("ThirtySecond", 0.125),
    ("Sixteenth", 0.25),
    ("Eighth", 0.5),
    ("Quarter", 1.0),
    ("Half", 2.0),
    ("One", 4.0),
    ("Two", 8.0),
    ("Four", 16.0),
];

pub struct LoadedInfo {
    pub path: PathBuf,
    pub tags: Tags,
    pub waveform: Arc<Waveform>,
    pub track_id: u64,
    pub grid_source: Option<GridSource>,
    pub locked: bool,
    pub cues: [Option<Cue>; NUM_HOT_CUES],
    /// Cue points are to be detected once the stems are ready (cleared by any cue edit).
    pub cues_pending: bool,
}

impl LoadedInfo {
    pub fn title(&self) -> String {
        self.tags
            .title
            .clone()
            .unwrap_or_else(|| self.path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default())
    }
}

#[derive(Default)]
pub struct DeckUi {
    pub info: Option<LoadedInfo>,
    pub loading: Option<PathBuf>,
    pub error: Option<String>,
    /// Stem separation progress (0..1) while it runs for the loaded track.
    pub stems_progress: Option<f32>,
    /// What produced the attached stems ("HT-Demucs", "prepared stems").
    pub stems_source: Option<String>,
    /// Why this track has no stems (no HT-Demucs model, or it failed).
    pub stems_missing: Option<&'static str>,
    /// `LoopInOut` set a loop start and is waiting for the end.
    pub loop_in_pending: bool,
    /// Per-stem loudness of the loaded track, for the waveform lanes (once separated).
    pub stem_wave: Option<std::sync::Arc<crate::stems::StemWave>>,
    /// The loaded track's sections (intro, verse, chorus…), once found from its stems.
    pub sections: Option<Arc<Vec<crate::stems::Section>>>,
    /// The loaded track as the engine has it (to request its stems again).
    pub track: Option<Arc<dj_engine::track::Track>>,
    /// Bar/beat last reported over OSC.
    last_beat: i64,
    /// A jog wheel nudging the playing track: until when (it stops shortly after the wheel does).
    jog_until: Option<Instant>,
    /// You picked where this track starts (a hot cue pad or the Cue button since it loaded): a
    /// mix brings it in from there.
    pub cue_chosen: bool,
}

enum LoadJob {
    Track { deck: usize, path: PathBuf, overrides: Option<TrackOverrides>, match_loudness: bool, detect: dj_library::DetectOptions },
    Sample { deck: usize, lane: usize, path: PathBuf },
}

enum LoadResult {
    Track { deck: usize, path: PathBuf, result: Result<LoadedTrack, String> },
    Sample { deck: usize, lane: usize, path: PathBuf, result: Result<Arc<dj_engine::track::AudioData>, String> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Value,
    Switch,
    Trigger,
    Hold,
    Step,
    Bend,
    Index,
}

/// What kind of control an address is (for MIDI learn and feedback).
pub fn addr_kind(addr: &str) -> Kind {
    let last = addr.rsplit('/').next().unwrap_or("");
    let l = last.to_ascii_lowercase();
    if l.starts_with("cue") && l.len() == 4 && l.as_bytes()[3].is_ascii_digit() {
        return Kind::Trigger;
    }
    match l.as_str() {
        "playpause" | "gridstart" | "temposync" | "tempomaster" | "loop" | "loopin" | "loopout" | "loopexit"
        | "skipbeatback" | "skipbeatforward" | "skipbarback" | "skipbarforward" | "pitchdown" | "pitchup"
        | "pitchreset" | "phasereset" | "eject" | "loadselected" | "gridstarthere" | "master" | "resend" | "save" | "loopinout"
        | "previous" | "next" | "tap" => Kind::Trigger,
        "playing" | "cuemomentary" | "slip" | "cueslip" | "faderstart" | "speedhalf" | "speeddouble" | "reverse"
        | "slipdotted" | "slipth" | "sliptriplet" | "pitchpreserve" | "mute" | "solo" | "fx" | "on" | "limiter"
        | "wallclock" | "sync" | "record" | "quantizeon" | "bypassall" | "stemdrums" | "stembass"
        | "stemvocals" | "stemother" | "acapella" | "instrumental" => Kind::Switch,
        "cue" | "backspin" | "phaseminus" | "phaseplus" | "trigger" => Kind::Hold,
        "beatskip" | "barskip" | "loopmovebeat" | "loopmovebar" | "pitchstep" | "bpmstep" | "browse" | "jog" | "loopsizestep" => Kind::Step,
        "phasebend" | "bend" => Kind::Bend,
        "quantize" | "speed" | "output" | "lane" | "pattern" => Kind::Index,
        _ if SLIP_LOOPS.iter().any(|(n, _)| n.eq_ignore_ascii_case(last)) => Kind::Hold,
        _ => Kind::Value,
    }
}

/// Deck stem switches (`/deck/N/stemvocals` etc., on = audible) -> stem index.
pub fn stem_switch(name: &str) -> Option<usize> {
    match name {
        "stemdrums" => Some(0),
        "stembass" => Some(1),
        "stemvocals" => Some(2),
        "stemother" => Some(3),
        _ => None,
    }
}

/// A tempo fader position (0..1) snapped to whole steps of `step_pct` percent (Settings → Tempo
/// fader steps; 0 = continuous), for the on-screen fader and controllers alike.
pub fn snap_tempo(v: f32, range: dj_engine::deck::TempoRange, step_pct: f32) -> f32 {
    if step_pct <= 0.0 {
        return v;
    }
    let fader = (v as f64 * 2.0 - 1.0).clamp(-1.0, 1.0);
    let pct = (range.speed(fader) - 1.0) * 100.0;
    let snapped = (pct / step_pct as f64).round() * step_pct as f64;
    ((range.fader_for_speed(1.0 + snapped / 100.0).clamp(-1.0, 1.0) + 1.0) / 2.0) as f32
}

/// Why a music folder can't be read, and what to do about it.
fn folder_error(dir: &Path, e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::NotFound => format!("Folder not found: {} (is the drive connected?)", dir.display()),
        std::io::ErrorKind::PermissionDenied if cfg!(target_os = "macos") => format!(
            "No access to {}: use Import folder to choose it, or allow dj in System Settings → Privacy & Security → Files and Folders",
            dir.display()
        ),
        _ => format!("Can't read {}: {e}", dir.display()),
    }
}

/// Deck stem levels (`/deck/N/stemvocalslevel` etc., 0..1) -> stem index.
pub fn stem_level(name: &str) -> Option<usize> {
    stem_switch(name.strip_suffix("level")?)
}

pub const STEMS_ACAPELLA: [f32; 4] = [0.0, 0.0, 1.0, 0.0];
pub const STEMS_INSTRUMENTAL: [f32; 4] = [1.0, 1.0, 0.0, 1.0];

fn arg_f(args: &[OscType]) -> Option<f32> {
    args.iter().find_map(|a| match a {
        OscType::Float(f) if f.is_finite() => Some(*f),
        OscType::Double(d) if d.is_finite() => Some(*d as f32),
        OscType::Int(i) => Some(*i as f32),
        OscType::Long(l) => Some(*l as f32),
        OscType::Bool(b) => Some(*b as i32 as f32),
        _ => None,
    })
}

fn arg_ints(args: &[OscType]) -> Vec<i32> {
    args.iter()
        .filter_map(|a| match a {
            OscType::Int(i) => Some(*i),
            OscType::Float(f) => Some(f.round() as i32),
            OscType::Long(l) => Some(*l as i32),
            _ => None,
        })
        .collect()
}

pub struct Recorder {
    pub path: PathBuf,
    pub started: Instant,
    thread: Option<std::thread::JoinHandle<()>>,
}

pub struct Core {
    pub ctl: Controller,
    pub sample_rate: f32,
    pub max_block: usize,
    pub settings: Settings,
    pub session: Session,
    pub profile: MappingProfile,
    pub trackdb: TrackDb,
    pub snap: Snapshot,
    pub decks: [DeckUi; NUM_DECKS],
    pub fx_live: [[Option<FxLive>; MAX_SLOTS]; NUM_DECKS],
    pub catalog_native: Vec<CatalogEntry>,
    pub catalog_vst3: Vec<CatalogEntry>,
    pub vst3_errors: Vec<(PathBuf, String)>,
    pub vst3_scanning: bool,
    vst3_rx: Option<Receiver<ScanCache>>,
    pub library: Vec<Entry>,
    /// Keeps the computer awake while dj runs (Settings → Decks).
    awake: crate::awake::KeepAwake,
    /// Preparing library tracks in the background (see `prepare_tracks`).
    pub preparer: Option<crate::profile::Preparer>,
    /// Tracks whose profile is in (or not, `None`) as read from disk, by path.
    pub profiles: HashMap<PathBuf, Option<std::sync::Arc<crate::profile::Profile>>>,
    /// How well library tracks mix after a deck's track: (deck track id, library path).
    pub suggestions: HashMap<(u64, PathBuf), Option<automix::Suggestion>>,
    pub library_scanning: bool,
    library_rx: Option<Receiver<Vec<Entry>>>,
    pub browser_selected: Option<usize>,
    /// Tracks loaded this session (for the HIST view).
    pub history: Vec<PathBuf>,
    pub browser_query: String,
    /// Crates (dj's own and imported from Serato / rekordbox), and the one the browser shows.
    pub crates: Vec<dj_library::crates::Crate>,
    pub browser_crate: Option<usize>,
    loader_tx: Sender<LoadJob>,
    loader_rx: Receiver<LoadResult>,
    pub stems: crate::stems::StemsWorker,
    pub learn: bool,
    pub last_touched: Option<String>,
    /// Last address a MIDI mapping changed, and when (the mixer shows that fader function).
    pub midi_touched: Option<(String, std::time::Instant)>,
    /// A track asked onto a deck that's playing: it waits for you to confirm (deck, track).
    pub pending_load: Option<(usize, PathBuf)>,
    /// 14-bit controls (faders that send a coarse CC 0–31 and then its fine half, CC + 32): the
    /// last coarse value per channel and CC, and whether a fine half has been seen for it.
    cc_coarse: [[u8; 32]; 16],
    cc_fine: [[bool; 32]; 16],
    pub status: VecDeque<(Instant, String)>,
    pub osc_in: Option<Receiver<(String, Vec<OscType>)>>,
    pub osc_out: Option<crate::osc::Sender>,
    pub midi: crate::midi::Midi,
    feedback: HashMap<String, Vec<OscType>>,
    last_feedback: Instant,
    last_time_feedback: Instant,
    pub recorder: Option<Recorder>,
    pub beat_flash: (i64, u8, Instant),
    pub midi_led: HashMap<MidiSource, u8>,
    /// Output channels of the open audio device (set at startup).
    pub audio_channels: usize,
    dirty_since: Option<Instant>,
    /// The auto-mix in progress, if any.
    pub automix: Option<automix::AutoMix>,
    /// After a Now mix: the new track's vocals coming back on its next vocal phrase.
    pub vocal_return: Option<automix::VocalReturn>,
    /// Auto-DJ, when on (see `set_autodj`).
    pub autodj: Option<automix::AutoDj>,
    /// A deck whose mix profile is to be saved (deck, track, since).
    profile_due: Option<(usize, u64, Instant)>,
    /// The deck easing back to its own tempo after a mix (see `tempo_home_tick`).
    pub tempo_home: Option<automix::TempoHome>,
    /// A mix's key shift, undone on the new track's next phrase line.
    pub key_home: Option<automix::KeyHome>,
    /// Echo-out inserts to remove once their tails have rung out: (channel, slot, clock beat).
    pub echo_cleanup: Vec<(usize, usize, f64)>,
    /// The two tracks the deck-order tip was last worked out for.
    order_hinted: Option<Vec<u64>>,
    /// MIDI pick-up: controls dj moved itself (lowercase address -> last MIDI value seen).
    pickup: HashMap<String, Option<f32>>,
    /// Each deck's loop size in beats (the loop buttons, and hot cue pads in Loop mode).
    pub loop_size: [f64; NUM_DECKS],
}

impl Core {
    pub fn new(ctl: Controller, sample_rate: f32, max_block: usize, settings: Settings, session: Session) -> Self {
        let loop_size = [settings.default_loop_beats; NUM_DECKS];
        // The cache of the small-model/placeholder stems is gone with them: free its disk space.
        let _ = std::fs::remove_dir_all(crate::settings::config_dir().join("stems-cache"));
        let (job_tx, job_rx) = unbounded::<LoadJob>();
        let (res_tx, res_rx) = unbounded::<LoadResult>();
        for _ in 0..2 {
            let job_rx = job_rx.clone();
            let res_tx = res_tx.clone();
            std::thread::Builder::new()
                .name("loader".into())
                .spawn(move || {
                    while let Ok(job) = job_rx.recv() {
                        let r = match job {
                            LoadJob::Track { deck, path, overrides, match_loudness, detect } => LoadResult::Track {
                                deck,
                                result: dj_library::load(&path, overrides.as_ref(), match_loudness, detect).map_err(|e| e.to_string()),
                                path,
                            },
                            LoadJob::Sample { deck, lane, path } => LoadResult::Sample {
                                deck,
                                lane,
                                result: dj_library::load_sample(&path).map_err(|e| e.to_string()),
                                path,
                            },
                        };
                        if res_tx.send(r).is_err() {
                            break;
                        }
                    }
                })
                .expect("spawn loader");
        }
        let midi = crate::midi::Midi::new(&settings);
        let mut core = Core {
            ctl,
            sample_rate,
            max_block,
            settings,
            session,
            profile: MappingProfile::load(),
            trackdb: TrackDb::load(),
            snap: Snapshot::default(),
            decks: Default::default(),
            fx_live: Default::default(),
            catalog_native: fxcat::native_entries(),
            catalog_vst3: Vec::new(),
            vst3_errors: Vec::new(),
            vst3_scanning: false,
            vst3_rx: None,
            library: Vec::new(),
            awake: Default::default(),
            library_scanning: false,
            library_rx: None,
            browser_selected: None,
            history: Vec::new(),
            crates: Vec::new(),
            browser_crate: None,
            browser_query: String::new(),
            loader_tx: job_tx,
            loader_rx: res_rx,
            stems: crate::stems::StemsWorker::new(),
            learn: false,
            last_touched: None,
            midi_touched: None,
            pending_load: None,
            cc_coarse: [[0; 32]; 16],
            cc_fine: [[false; 32]; 16],
            status: VecDeque::new(),
            osc_in: None,
            osc_out: None,
            midi,
            feedback: HashMap::new(),
            last_feedback: Instant::now(),
            last_time_feedback: Instant::now(),
            recorder: None,
            beat_flash: (0, 1, Instant::now()),
            midi_led: HashMap::new(),
            audio_channels: 2,
            dirty_since: None,
            automix: None,
            vocal_return: None,
            autodj: None,
            profile_due: None,
            preparer: None,
            profiles: HashMap::new(),
            suggestions: HashMap::new(),
            tempo_home: None,
            key_home: None,
            echo_cleanup: Vec::new(),
            order_hinted: None,
            pickup: HashMap::new(),
            loop_size,
        };
        // VST3 catalog from cache immediately; refresh in the background.
        let cache: ScanCache = crate::settings::load_json(&ScanCache::path());
        core.apply_scan(cache);
        let mut library: Vec<dj_library::scan::Entry> = crate::settings::load_json(&crate::settings::config_dir().join("library-cache.json"));
        core.fill_detected(&mut library);
        core.library = library;
        core.crates = crate::settings::load_json(&Self::crates_path());
        core.start_osc();
        core
    }

    // ------------------------------------------------------------ basics

    pub fn log(&mut self, msg: impl Into<String>) {
        let m = msg.into();
        eprintln!("{m}");
        self.status.push_back((Instant::now(), m));
        while self.status.len() > 50 {
            self.status.pop_front();
        }
    }

    pub fn send(&mut self, cmd: Command) {
        if let Err(c) = self.ctl.send(cmd) {
            // Queue full: should never happen at UI rates. Drop it on this thread.
            drop(c);
            self.log("engine command queue full");
        }
    }

    fn deck_cmd(&mut self, d: usize, cmd: DeckCmd) {
        self.send(Command::Deck { deck: d as u8, cmd });
    }

    fn mark_dirty(&mut self) {
        self.dirty_since.get_or_insert_with(Instant::now);
    }

    pub fn deck_count(&self) -> usize {
        self.settings.deck_count.clamp(2, 4)
    }

    pub fn key_text(&self, k: Option<Key>) -> String {
        match k {
            None => String::new(),
            Some(k) => match self.settings.key_notation {
                KeyNotation::Camelot => k.camelot(),
                KeyNotation::Musical => k.musical(),
                KeyNotation::OpenKey => k.open_key(),
            },
        }
    }

    /// Key of a deck including its pitch shift (key lock on) or tempo change.
    pub fn deck_key(&self, d: usize) -> Option<Key> {
        let k = self.decks[d].info.as_ref()?.tags.key?;
        let v = &self.snap.decks[d];
        let shift = if v.keylock { v.semitones } else { v.semitones + 12.0 * v.speed.max(0.01).log2() };
        Some(k.transpose(shift.round() as i32))
    }

    /// The deck harmonic hints and Fits go by: the one you hear. The master when it plays with
    /// its fader up (not muted), else the loudest deck that does (the master can be a deck
    /// left looping silently, or one a mix took out); with nothing up (or an external mixer,
    /// where dj can't see the faders), the master, else the first deck playing.
    pub fn reference_deck(&self) -> Option<usize> {
        let n = self.deck_count();
        let playing = |d: usize| self.snap.decks[d].playing && self.snap.decks[d].loaded;
        let external = self.external_routing();
        let up = |d: usize| {
            let p = &self.session.channels[d].params;
            playing(d) && !external && p.volume > 0.05 && !p.mute
        };
        let master = match self.snap.clock.owner {
            ClockOwner::Deck(m) if (m as usize) < n => Some(m as usize),
            _ => None,
        };
        master
            .filter(|&m| up(m))
            .or_else(|| (0..n).filter(|&d| up(d)).max_by(|&a, &b| self.session.channels[a].params.volume.total_cmp(&self.session.channels[b].params.volume)))
            .or_else(|| master.filter(|&m| playing(m) || external))
            .or_else(|| (0..n).find(|&d| playing(d)))
    }

    /// Key used as the reference for harmonic hints: that of [`Core::reference_deck`].
    pub fn reference_key(&self) -> Option<Key> {
        self.deck_key(self.reference_deck()?)
    }

    // ------------------------------------------------------------ startup / resync

    /// Push the whole session to a fresh engine (startup and after an audio
    /// device change).
    pub fn sync_engine(&mut self) {
        let s = self.settings.clone();
        self.send(Command::FaderCurve(s.fader_curve));
        self.send(Command::Crossovers { low_hz: s.crossover_low_hz, high_hz: s.crossover_high_hz });
        self.send(Command::ExtStages(self.ext_stages()));
        self.send(Command::SplitCue(s.audio.split_cue));
        let ses = self.session.clone();
        // Wallclock (a fixed tempo nobody can see) has no on-screen control any more: dj always
        // picks a playing deck as master, so a session saved with it on starts with it off.
        self.session.wallclock = false;
        self.send(Command::Wallclock(false));
        self.send(Command::SetBpm(ses.bpm));
        self.send(Command::GlobalQuantize(ses.global_quantize()));
        self.apply_master();
        for ch in 0..NUM_DECKS {
            let c = &ses.channels[ch];
            self.send(Command::Channel { ch: ch as u8, params: self.engine_params(ch, c.params) });
            self.send(Command::FaderStart { ch: ch as u8, on: c.fader_start });
            self.restore_chain(ch, c.fx.clone());
        }
        for d in 0..NUM_DECKS {
            let dm = ses.decks[d].clone();
            self.deck_cmd(d, DeckCmd::TempoRange(dm.tempo_range));
            self.deck_cmd(d, DeckCmd::KeyLock(dm.keylock));
            self.deck_cmd(d, DeckCmd::Quantize(dm.quantize));
            self.deck_cmd(d, DeckCmd::Slip(dm.slip));
            self.deck_cmd(d, DeckCmd::MomentaryCues(dm.momentary_cues));
            self.deck_cmd(d, DeckCmd::TapeStopMs(s.tape_stop_ms));
            self.deck_cmd(d, DeckCmd::DefaultLoopBeats(s.default_loop_beats));
            self.send(Command::DeckMode { deck: d as u8, mode: dm.mode });
            if dm.mode == DeckMode::Sampler {
                self.sync_sampler(d);
            } else if let Some(p) = dm.track {
                if p.exists() {
                    self.load_track(d, &p);
                }
            }
        }
    }

    fn sync_sampler(&mut self, d: usize) {
        let sm = self.session.decks[d].sampler.clone();
        for (l, lane) in sm.lanes.iter().enumerate() {
            self.send(Command::Sampler { deck: d as u8, cmd: SamplerCmd::Lane { lane: l as u8, params: lane.params } });
            if let Some(p) = &lane.sample {
                let _ = self.loader_tx.send(LoadJob::Sample { deck: d, lane: l, path: p.clone() });
            }
        }
        for (pi, pat) in sm.patterns.iter().enumerate() {
            for (l, steps) in pat.iter().enumerate() {
                for (s, st) in steps.iter().enumerate() {
                    if st.on || st.velocity != 1.0 {
                        self.send(Command::Sampler {
                            deck: d as u8,
                            cmd: SamplerCmd::Step { pattern: pi as u8, lane: l as u8, step: s as u8, value: *st },
                        });
                    }
                }
            }
        }
        self.send(Command::Sampler { deck: d as u8, cmd: SamplerCmd::Pattern(sm.pattern as u8) });
        self.send(Command::Sampler { deck: d as u8, cmd: SamplerCmd::Swing(sm.swing) });
        self.send(Command::Sampler { deck: d as u8, cmd: SamplerCmd::GlobalPitch(sm.global_pitch) });
        self.send(Command::Sampler { deck: d as u8, cmd: SamplerCmd::Speed(sm.speed) });
    }

    pub fn apply_master(&mut self) {
        let m = self.session.master.clone();
        self.send(Command::MasterGain(crate::model::MasterModel::knob_gain(m.volume)));
        self.send(Command::Limiter(m.limiter));
        let cue_gain = if self.settings.audio.cue_enabled { m.cue_volume * m.cue_volume * 1.5 } else { 0.0 };
        self.send(Command::CueGain(cue_gain));
        self.send(Command::CueMix(m.cue_mix));
        self.mark_dirty();
    }

    // ------------------------------------------------------------ tracks

    /// Load a track you asked for (the browser, a drop, a controller's LOAD): onto a deck that's
    /// playing only once you confirm (the alert on screen, or LOAD pressed again for the same
    /// track). dj's own loads (Auto-DJ, the session) call `load_track` directly.
    pub fn request_load(&mut self, d: usize, path: &Path) {
        if d >= NUM_DECKS {
            return;
        }
        if self.snap.decks[d].playing {
            if self.pending_load.as_ref().is_some_and(|(pd, pp)| *pd == d && pp == path) {
                self.confirm_load();
                return;
            }
            self.pending_load = Some((d, path.to_path_buf()));
            self.log(format!("Deck {} is playing: load anyway? (confirm, or press LOAD again)", d + 1));
            return;
        }
        self.pending_load = None;
        self.load_track(d, path);
    }

    /// The alert answered: load it after all.
    pub fn confirm_load(&mut self) {
        if let Some((d, p)) = self.pending_load.take() {
            self.load_track(d, &p);
        }
    }

    pub fn cancel_load(&mut self) {
        self.pending_load = None;
    }

    pub fn load_track(&mut self, d: usize, path: &Path) {
        if d >= NUM_DECKS {
            return;
        }
        let overrides = self.trackdb.tracks.get(&TrackDb::key(path)).cloned();
        self.stems.cancel(d);
        self.decks[d].stems_progress = None;
        self.decks[d].stems_source = None;
        self.decks[d].stems_missing = None;
        self.decks[d].stem_wave = None;
        self.decks[d].sections = None;
        self.decks[d].loop_in_pending = false;
        self.decks[d].cue_chosen = false;
        self.decks[d].loading = Some(path.to_path_buf());
        self.decks[d].error = None;
        let _ = self.loader_tx.send(LoadJob::Track {
            deck: d,
            path: path.to_path_buf(),
            overrides,
            match_loudness: self.settings.match_loudness,
            detect: dj_library::DetectOptions {
                prefer_detected_cues: self.settings.cue_points == crate::settings::CuePoints::Detected,
                // With stems on, cue points wait for the stems (better); otherwise from the mix.
                cues_from_mix: !self.settings.stems_enabled,
            },
        });
    }

    /// Detect this deck's cue points again (from its stems), merged into the current ones (see
    /// [`merge_cues`]).
    pub fn redetect_cues(&mut self, d: usize) {
        let Some(info) = self.decks[d].info.as_mut() else { return };
        info.cues_pending = true;
        let path = info.path.clone();
        self.request_stems(d, &path, true);
    }

    /// Ask the stems worker for this deck's stems (and cue points from them, if `detect_cues`).
    fn request_stems(&mut self, d: usize, path: &Path, detect_cues: bool) {
        let Some(track) = self.loaded_track(d) else { return };
        // (A detected grid's tempo map was measured on its tempo: if that's wrong, so is the map.)
        let tempo_from_beats = self.decks[d].info.as_ref().is_some_and(|i| i.grid_source == Some(GridSource::Detected));
        // (Progress shows once separation starts; prepared stems just arrive.)
        self.decks[d].stems_progress = None;
        self.decks[d].stems_missing = None;
        self.stems.request(crate::stems::Request {
            deck: d,
            track,
            path: path.to_path_buf(),
            demucs: self.settings.stems_demucs.clone().filter(|p| p.exists()),
            detect_cues,
            tempo_from_beats,
            prepared: Some(self.settings.stems_prepared_dir()),
            // (Again when cue points are wanted: they're placed on the sections, kept on disk.)
            sections: self.settings.song_sections && (self.decks[d].sections.is_none() || detect_cues),
        });
    }

    fn loaded_track(&self, d: usize) -> Option<Arc<dj_engine::track::Track>> {
        self.decks[d].track.clone()
    }

    /// Detected cue points arrived: use them unless cues were edited in the meantime.
    /// Put detected cue points on a deck's pads (see [`merge_cues`]): close to a cue it has (within
    /// a bar), the detected one replaces it; the others go on free pads; the rest stay.
    fn apply_detected_cues(&mut self, d: usize, found: Vec<Cue>) {
        let Some(info) = self.decks[d].info.as_mut() else { return };
        if !std::mem::take(&mut info.cues_pending) || found.is_empty() {
            return;
        }
        let bpm = self.snap.decks[d].track_bpm;
        let close = if bpm > 0.0 { 4.0 * 60.0 / bpm } else { 2.0 };
        let old = info.cues.clone();
        let (new, replaced, added, dropped) = merge_cues(&old, found, close);
        for i in 0..NUM_HOT_CUES {
            if new[i] != old[i] {
                let c = &new[i];
                self.deck_cmd(d, DeckCmd::SetCue { idx: i as u8, secs: c.as_ref().map(|c| c.secs), end_secs: c.as_ref().and_then(|c| c.end_secs) });
                self.persist_cue(d, i as u8, new[i].clone());
            }
        }
        let full = if dropped > 0 { format!(", {dropped} left out: pads full") } else { String::new() };
        self.log(format!("Deck {}: cue points detected ({replaced} replaced, {added} added{full})", d + 1));
    }

    pub fn eject(&mut self, d: usize) {
        self.decks[d].track = None;
        self.stems.cancel(d);
        self.decks[d].stems_progress = None;
        self.decks[d].stems_source = None;
        self.decks[d].stems_missing = None;
        self.decks[d].stem_wave = None;
        self.decks[d].sections = None;
        self.send(Command::Eject { deck: d as u8 });
        self.decks[d].info = None;
        self.session.decks[d].track = None;
        self.mark_dirty();
    }

    fn on_track_loaded(&mut self, d: usize, lt: LoadedTrack) {
        let locked = self.trackdb.tracks.get(&TrackDb::key(&lt.path)).map(|o| o.locked).unwrap_or(false);
        let info = LoadedInfo {
            path: lt.path.clone(),
            tags: lt.tags.clone(),
            waveform: lt.waveform.clone(),
            track_id: lt.track.id,
            grid_source: lt.grid_source,
            locked,
            cues: lt.track.cues.clone(),
            cues_pending: lt.detected.cues_pending && self.settings.stems_enabled,
        };
        self.save_detected(&lt);
        if let Some((k, sure)) = lt.detected.key {
            self.log(format!("Deck {}: no key tag; {} from the audio ({:.0} % sure)", d + 1, self.key_text(Some(k)), sure * 100.0));
        }
        self.decks[d].track = Some(lt.track.clone());
        self.send(Command::DeckMode { deck: d as u8, mode: DeckMode::Track });
        if self.settings.stems_enabled {
            self.request_stems(d, &lt.path, info.cues_pending);
        }
        self.send(Command::Load { deck: d as u8, track: lt.track, start_at_first_cue: self.settings.load_at_first_cue });
        // A new track starts with every stem on (the engine resets them): forget pick-up for the
        // old track's stem levels.
        let stems = format!("deck/{}/stem", d + 1);
        self.pickup.retain(|k, _| !k.starts_with(&stems));
        if self.settings.keylock_on_load {
            self.session.decks[d].keylock = true;
        }
        let kl = self.session.decks[d].keylock;
        self.deck_cmd(d, DeckCmd::KeyLock(kl));
        self.session.decks[d].mode = DeckMode::Track;
        self.session.decks[d].track = Some(lt.path.clone());
        *self.trackdb.play_counts.entry(TrackDb::key(&lt.path)).or_default() += 1;
        self.history.retain(|p| p != &lt.path);
        self.history.push(lt.path.clone());
        self.decks[d].info = Some(info);
        self.decks[d].loading = None;
        self.mark_dirty();
    }

    fn on_stems_update(&mut self, u: crate::stems::Update) {
        use crate::stems::Update;
        let current = |core: &Self, d: usize, id: u64| core.decks[d].info.as_ref().is_some_and(|i| i.track_id == id);
        match u {
            Update::Progress { deck, track_id, fraction } if current(self, deck, track_id) => {
                self.decks[deck].stems_progress = Some(fraction);
            }
            Update::Done { deck, track_id, stems, wave, source, cues } if current(self, deck, track_id) => {
                self.decks[deck].stems_progress = None;
                self.decks[deck].stem_wave = Some(wave);
                self.decks[deck].stems_source = Some(source);
                self.send(Command::AttachStems { deck: deck as u8, stems });
                if let Some(c) = cues {
                    self.apply_detected_cues(deck, c);
                }
                if !self.settings.song_sections {
                    self.save_profile(deck);
                }
            }
            Update::Sections { deck, track_id, found, cues, drums } if current(self, deck, track_id) => {
                if !found.sections.is_empty() {
                    self.decks[deck].sections = Some(Arc::new(found.sections.clone()));
                }
                self.check_grid_with_beats(deck, &found.beats, drums);
                if let Some(c) = cues {
                    self.apply_detected_cues(deck, c);
                }
                // (After the grid check moved it, if it did: the profile is the track as played.)
                self.save_profile_soon(deck);
            }
            Update::NoStems { deck, track_id, error, cues } if current(self, deck, track_id) => {
                self.decks[deck].stems_progress = None;
                self.decks[deck].stems_missing = Some(if error.is_some() { "stems failed" } else { "no stems: get HT-Demucs" });
                if let Some(e) = error {
                    self.log(format!("Deck {} stems: {e}", deck + 1));
                }
                if let Some(c) = cues {
                    self.apply_detected_cues(deck, c);
                }
            }
            _ => {} // for a track that is no longer loaded
        }
    }

    /// Set stem gains on a deck (0 = muted, 1 = on), in `Stem` order.
    fn set_stems(&mut self, d: usize, gains: [f32; 4]) -> Option<DeckCmd> {
        for (i, g) in gains.iter().enumerate().skip(1) {
            self.deck_cmd(d, DeckCmd::StemGain { stem: i as u8, gain: *g });
        }
        Some(DeckCmd::StemGain { stem: 0, gain: gains[0] })
    }

    /// Check the deck's grid against the beats All-In-One hears: half a beat off (no kicks to
    /// place it), or bars starting on the wrong beat (unless the cue points, on phrase starts,
    /// say where the bars are). A grid you set yourself, or one with a tempo map, is only reported
    /// on. The fix is for this load (the check runs again on the next). `drums`: the drum stem's
    /// grid, when the heard beats are too unsteady to settle a detected tempo.
    fn check_grid_with_beats(&mut self, d: usize, beats: &[crate::stems::Beat], drums: Option<dj_engine::track::BeatGrid>) {
        let Some(track) = self.decks[d].track.clone() else { return };
        let Some(info) = self.decks[d].info.as_ref() else { return };
        let Some(grid) = track.grid else { return };
        let heard: Vec<(f64, f32)> = beats.iter().map(|b| (b.secs, b.downbeat)).collect();
        let map = track.tempo_map.clone();
        // A detected tempo the heard beats steadily disagree with: theirs (the detector took
        // double, half or 3:2 of the beat); when they wander, the drum stem's. Kept, and shown
        // in the library. (Also with a tempo map: one measured on a wrong tempo is wrong too.
        // The corrected grid replaces it.)
        if info.grid_source == Some(GridSource::Detected) {
            let from_beats = dj_library::analysis::tempo_from_beats(&grid, &heard);
            let why = if from_beats.is_some() { "the steady beats All-In-One hears" } else { "the drum stem (All-In-One's beats wander)" };
            if let Some(fixed) = crate::stems::corrected_grid(&grid, &heard, drums.as_ref()) {
                self.deck_cmd(d, DeckCmd::SetGrid(Some(fixed)));
                self.keep_detected_grid(d, fixed);
                self.log(format!("Deck {}: BPM {:.1} → {:.1}, from {why}", d + 1, grid.bpm, fixed.bpm));
                return;
            }
        }
        let spb = grid.seconds_per_beat();
        let beat_at = |t: f64| map.as_ref().map_or_else(|| grid.beat_at(t), |m| m.beat_at(t));
        let Some(check) = dj_library::analysis::check_beats(beat_at, spb, &heard) else { return };
        let (half, bars, cues_say_bars) = dj_library::analysis::grid_shift(&check, beat_at, &info.cues);
        let mine = info.grid_source == Some(GridSource::User);
        let pct = (check.agree * 100.0).round();
        let what = match (check.offbeat, bars != 0.0) {
            (false, false) => {
                if check.agree < 0.5 {
                    self.log(format!("Deck {}: the grid doesn't follow the beats All-In-One hears ({pct:.0} % agree): check its BPM", d + 1));
                } else {
                    let bars = if check.bar_shift.is_some() && cues_say_bars { ", bars as the cues say" } else { "" };
                    self.log(format!("Deck {}: grid checked: {pct:.0} % of the beats All-In-One hears are on it{bars}", d + 1));
                }
                return;
            }
            (true, false) => "half a beat off".to_string(),
            (false, true) => format!("its bars start {bars:.0} beat{} later", if bars == 1.0 { "" } else { "s" }),
            (true, true) => format!("half a beat off, and its bars start {bars:.0} beat{} later", if bars == 1.0 { "" } else { "s" }),
        };
        if mine || map.is_some() {
            self.log(format!("Deck {}: All-In-One hears the grid {what}; left as it is ({})", d + 1, if mine { "your grid" } else { "a tempo map" }));
            return;
        }
        let fixed = dj_engine::track::BeatGrid { bpm: grid.bpm, anchor_secs: grid.anchor_secs + (half + bars) * spb };
        self.deck_cmd(d, DeckCmd::SetGrid(Some(fixed)));
        self.log(format!("Deck {}: grid corrected from the beats All-In-One hears: {what}", d + 1));
    }

    /// Keep how much of a track has vocals (from its stems), and show it in the library.
    fn keep_vocals(&mut self, path: &Path, vocals: u8) {
        let o = self.trackdb.tracks.entry(TrackDb::key(path)).or_default();
        if o.vocals != Some(vocals) {
            o.vocals = Some(vocals);
            self.mark_dirty();
        }
        if let Some(e) = self.library.iter_mut().find(|e| e.path == path) {
            e.vocals = Some(vocals);
        }
    }

    /// Keep a detected grid corrected on a deck (unless the track is locked), and show its BPM in
    /// the library.
    fn keep_detected_grid(&mut self, d: usize, grid: dj_engine::track::BeatGrid) {
        let Some(info) = self.decks[d].info.as_ref() else { return };
        if info.locked {
            return;
        }
        let path = info.path.clone();
        let o = self.trackdb.tracks.entry(TrackDb::key(&path)).or_default();
        o.grid = Some(grid);
        o.grid_detected = true;
        if let Some(e) = self.library.iter_mut().find(|e| e.path == path) {
            e.bpm = Some(grid.bpm);
        }
        self.mark_dirty();
    }

    /// Keep a deck's mix profile once its track is fully analysed, so it can be suggested and
    /// mixed while it isn't loaded. (A moment later when the grid was just corrected: the engine
    /// has it then.)
    fn save_profile_soon(&mut self, d: usize) {
        self.profile_due = Some((d, self.decks[d].info.as_ref().map_or(0, |i| i.track_id), Instant::now()));
    }

    fn save_profile(&mut self, d: usize) {
        let Some(info) = self.decks[d].info.as_ref() else { return };
        let path = info.path.clone();
        let Some((bars, cue_bars)) = self.deck_bars(d) else { return };
        let sections = self.decks[d].sections.as_deref().cloned().unwrap_or_default();
        let p = crate::profile::Profile::new(&path, self.snap.decks[d].track_bpm, &bars, cue_bars, &sections);
        if let Err(e) = crate::profile::write(&self.settings.stems_prepared_dir(), &path, &p) {
            eprintln!("profile: {e}");
        }
        if let Some(v) = p.vocals() {
            self.keep_vocals(&path, v);
        }
        self.profiles.insert(path, Some(std::sync::Arc::new(p)));
    }

    /// Prepare tracks for mixing in the background: stems, sections and mix profiles (see
    /// `profile::prepare_track`), already prepared ones skipped.
    pub fn prepare_tracks(&mut self, paths: Vec<PathBuf>) {
        if let Some(p) = self.preparer.take() {
            p.stop();
        }
        let n = paths.len();
        let tracks = paths.into_iter().map(|p| (p.clone(), self.trackdb.tracks.get(&TrackDb::key(&p)).cloned())).collect();
        let demucs = self.settings.stems_demucs.clone().filter(|p| p.exists());
        if demucs.is_none() {
            self.log("Preparing without HT-Demucs: mix profiles only (no stems or sections). Download it in Settings → Stems.");
        }
        self.preparer = Some(crate::profile::Preparer::start(tracks, demucs, self.settings.stems_prepared_dir()));
        self.log(format!("Preparing {n} tracks in the background"));
    }

    pub fn stop_preparing(&mut self) {
        if let Some(p) = &self.preparer {
            p.stop();
        }
    }

    fn prepare_tick(&mut self) {
        if let Some((d, id, at)) = self.profile_due {
            if at.elapsed().as_millis() > 300 {
                self.profile_due = None;
                if self.decks[d].info.as_ref().is_some_and(|i| i.track_id == id) {
                    self.save_profile(d);
                }
            }
        }
        let Some(st) = self.preparer.as_ref().map(|p| p.state()) else { return };
        // Keys found while preparing (tracks without a key tag), and detected tempos the heard
        // beats corrected: kept, and shown in the library.
        for f in self.preparer.as_ref().map(|p| p.take_found()).unwrap_or_default() {
            let o = self.trackdb.tracks.entry(TrackDb::key(&f.path)).or_default();
            if let Some(k) = f.key {
                o.key = Some(k);
            }
            let grid = f.grid.filter(|_| !o.locked);
            if let Some(g) = grid {
                o.grid = Some(g);
                o.grid_detected = true;
            }
            if let Some(v) = f.vocals {
                o.vocals = Some(v);
            }
            if let Some(e) = self.library.iter_mut().find(|e| e.path == f.path) {
                e.key = e.key.or(f.key);
                e.vocals = f.vocals.or(e.vocals);
                if let Some(g) = grid {
                    e.bpm = Some(g.bpm);
                }
            }
            self.mark_dirty();
        }
        if st.finished {
            self.preparer = None;
            // Profiles read before may have changed, and with them the scores.
            self.profiles.clear();
            self.suggestions.clear();
            let failed = if st.failed > 0 { format!(", {} failed (see the terminal)", st.failed) } else { String::new() };
            self.log(format!("Prepared {} of {} tracks ({} already were){failed}", st.prepared, st.total, st.done - st.prepared - st.failed));
        }
    }

    /// A library track's mix profile, if it has one (read from disk once).
    pub fn profile(&mut self, path: &Path) -> Option<std::sync::Arc<crate::profile::Profile>> {
        if !self.profiles.contains_key(path) {
            let p = crate::profile::read(&self.settings.stems_prepared_dir(), path).map(std::sync::Arc::new);
            self.profiles.insert(path.to_path_buf(), p);
        }
        self.profiles.get(path).cloned().flatten()
    }

    /// Save what loading detected (grid, energy, cue points from the mix) so the next load and
    /// the library have it; untagged library entries show the detected BPM and energy.
    fn save_detected(&mut self, lt: &LoadedTrack) {
        let det = &lt.detected;
        if !det.grid && det.energy.is_none() && !det.cues && det.key.is_none() {
            return;
        }
        let key = TrackDb::key(&lt.path);
        let o = self.trackdb.tracks.entry(key).or_default();
        if det.grid {
            o.grid = lt.track.grid;
            o.grid_detected = true;
        }
        if det.energy.is_some() {
            o.energy = det.energy;
        }
        if det.cues {
            o.cues = Some(lt.track.cues.to_vec());
        }
        if let Some((k, _)) = det.key {
            o.key = Some(k);
        }
        if let Some(e) = self.library.iter_mut().find(|e| e.path == lt.path) {
            if e.key.is_none() {
                e.key = det.key.map(|(k, _)| k);
            }
            if e.bpm.is_none() {
                e.bpm = lt.track.grid.map(|g| g.bpm);
            }
            if e.energy.is_none() {
                e.energy = det.energy;
            }
        }
        self.mark_dirty();
    }

    /// Library entries without BPM / energy / key tags get the values detected earlier.
    fn fill_detected(&self, library: &mut [dj_library::scan::Entry]) {
        for e in library.iter_mut().filter(|e| e.bpm.is_none() || e.energy.is_none() || e.key.is_none() || e.vocals.is_none()) {
            if let Some(o) = self.trackdb.tracks.get(&TrackDb::key(&e.path)) {
                if e.bpm.is_none() && o.grid_detected {
                    e.bpm = o.grid.map(|g| g.bpm);
                }
                if e.energy.is_none() {
                    e.energy = o.energy;
                }
                if e.key.is_none() {
                    e.key = o.key;
                }
                if e.vocals.is_none() {
                    e.vocals = o.vocals;
                }
            }
        }
    }

    fn persist_cue(&mut self, d: usize, idx: u8, cue: Option<Cue>) {
        let Some(info) = self.decks[d].info.as_mut() else { return };
        if let Some(slot) = info.cues.get_mut(idx as usize) {
            *slot = cue;
        }
        info.cues_pending = false; // an edit wins over detection still to come
        if info.locked {
            return; // Locked tracks: cues are for this load only.
        }
        let key = TrackDb::key(&info.path);
        let cues = info.cues.to_vec();
        self.trackdb.tracks.entry(key).or_default().cues = Some(cues);
        self.mark_dirty();
    }

    pub fn set_track_locked(&mut self, d: usize, locked: bool) {
        if let Some(info) = self.decks[d].info.as_mut() {
            info.locked = locked;
            let key = TrackDb::key(&info.path);
            self.trackdb.tracks.entry(key).or_default().locked = locked;
            self.mark_dirty();
        }
    }

    /// Persist a grid edit (e.g. "grid start here" or BPM correction).
    pub fn set_grid(&mut self, d: usize, grid: dj_engine::track::BeatGrid) {
        self.deck_cmd(d, DeckCmd::SetGrid(Some(grid)));
        if let Some(info) = self.decks[d].info.as_mut() {
            info.grid_source = Some(GridSource::User);
            let key = TrackDb::key(&info.path);
            self.trackdb.tracks.entry(key).or_default().grid = Some(grid);
            self.mark_dirty();
        }
    }

    pub fn grid_start_here(&mut self, d: usize) {
        let v = self.snap.decks[d];
        if v.has_grid {
            self.set_grid(d, dj_engine::track::BeatGrid { bpm: v.track_bpm, anchor_secs: v.position_secs });
        }
    }

    pub fn nudge_grid(&mut self, d: usize, ms: f64) {
        let v = self.snap.decks[d];
        if v.has_grid {
            // anchor = position - beat * spb
            let spb = 60.0 / v.track_bpm;
            let anchor = v.position_secs - v.beat * spb + ms / 1000.0;
            self.set_grid(d, dj_engine::track::BeatGrid { bpm: v.track_bpm, anchor_secs: anchor });
        }
    }

    // ------------------------------------------------------------ library & scans

    /// Add a music folder to the library (if it's readable) and scan it.
    pub fn add_library_folder(&mut self, dir: PathBuf) {
        // "~/Music" as typed in Settings.
        let dir = match dir.to_str().and_then(|s| s.strip_prefix("~/")) {
            Some(rest) => dirs::home_dir().map(|h| h.join(rest)).unwrap_or(dir),
            None => dir,
        };
        if let Err(e) = std::fs::read_dir(&dir) {
            self.log(folder_error(&dir, &e));
            return;
        }
        if !self.settings.library_folders.contains(&dir) {
            self.settings.library_folders.push(dir.clone());
            self.mark_dirty();
        }
        self.log(format!("Scanning {}…", dir.display()));
        self.rescan_library();
    }

    pub fn rescan_library(&mut self) {
        if self.library_scanning {
            return;
        }
        // Say which folders can't be read instead of silently finding nothing.
        for root in self.settings.library_folders.clone() {
            if let Err(e) = std::fs::read_dir(&root) {
                self.log(folder_error(&root, &e));
            }
        }
        let roots = self.settings.library_folders.clone();
        let cache = self.library.clone();
        // (Crates' tracks outside the library folders are listed too.)
        let extra: Vec<PathBuf> = self.crates.iter().flat_map(|c| c.tracks.iter().cloned()).collect();
        let (tx, rx) = unbounded();
        std::thread::spawn(move || {
            let _ = tx.send(dj_library::scan::scan_with(&roots, &extra, &cache));
        });
        self.library_rx = Some(rx);
        self.library_scanning = true;
    }

    pub fn rescan_vst3(&mut self) {
        if self.vst3_scanning || !self.settings.vst3_enabled {
            return;
        }
        let extra = self.settings.vst3_paths.clone();
        let (tx, rx) = unbounded();
        std::thread::spawn(move || {
            let _ = tx.send(fxcat::scan_vst3(&extra));
        });
        self.vst3_rx = Some(rx);
        self.vst3_scanning = true;
    }

    fn apply_scan(&mut self, cache: ScanCache) {
        self.catalog_vst3 = if self.settings.vst3_enabled { fxcat::vst3_entries(&cache) } else { Vec::new() };
        self.vst3_errors =
            cache.bundles.iter().filter_map(|(p, _, r)| r.as_ref().err().map(|e| (p.clone(), e.clone()))).collect();
    }

    /// The browser's list: the library, or the selected crate's tracks (in the crate's order;
    /// those not found are left out), matching the search.
    pub fn filtered_library(&self) -> Vec<usize> {
        match self.browser_crate.and_then(|c| self.crates.get(c)) {
            Some(c) => {
                let at: HashMap<&Path, usize> = self.library.iter().enumerate().map(|(i, e)| (e.path.as_path(), i)).collect();
                c.tracks.iter().filter_map(|p| at.get(p.as_path()).copied()).filter(|&i| self.library[i].matches(&self.browser_query)).collect()
            }
            None => self.library.iter().enumerate().filter(|(_, e)| e.matches(&self.browser_query)).map(|(i, _)| i).collect(),
        }
    }

    // ------------------------------------------------------------ crates

    pub fn crates_path() -> PathBuf {
        crate::settings::config_dir().join("crates.json")
    }

    fn save_crates(&mut self) {
        self.crates.sort_by_key(|c| c.name.to_lowercase());
        let _ = crate::settings::save_json_later(&Self::crates_path(), &self.crates);
    }

    /// Where crate `name` now sits (after sorting).
    fn crate_index(&self, name: &str) -> Option<usize> {
        self.crates.iter().position(|c| c.name == name)
    }

    /// A new, empty crate of yours (a name already taken gets a number); shown in the browser.
    pub fn new_crate(&mut self, name: &str) -> usize {
        let base = if name.trim().is_empty() { "New crate" } else { name.trim() };
        let mut name = base.to_string();
        let mut n = 2;
        while self.crate_index(&name).is_some() {
            name = format!("{base} {n}");
            n += 1;
        }
        self.crates.push(dj_library::crates::Crate { name: name.clone(), ..Default::default() });
        self.save_crates();
        let i = self.crate_index(&name).unwrap_or(0);
        self.browser_crate = Some(i);
        i
    }

    pub fn rename_crate(&mut self, i: usize, name: &str) {
        let name = name.trim();
        if name.is_empty() || i >= self.crates.len() || self.crate_index(name).is_some_and(|j| j != i) {
            return;
        }
        self.crates[i].name = name.to_string();
        let shown = self.browser_crate == Some(i);
        self.save_crates();
        if shown {
            self.browser_crate = self.crate_index(name);
        }
    }

    pub fn delete_crate(&mut self, i: usize) {
        if i >= self.crates.len() {
            return;
        }
        let name = self.crates.remove(i).name;
        self.browser_crate = match self.browser_crate {
            Some(c) if c == i => None,
            Some(c) if c > i => Some(c - 1),
            c => c,
        };
        self.save_crates();
        self.log(format!("Crate \"{name}\" deleted (its tracks stay in the library)"));
    }

    /// Add tracks to a crate (those already in it stay where they are).
    pub fn add_to_crate(&mut self, i: usize, paths: &[PathBuf]) {
        let Some(c) = self.crates.get_mut(i) else { return };
        let mut added = 0;
        for p in paths {
            if !c.tracks.contains(p) {
                c.tracks.push(p.clone());
                added += 1;
            }
        }
        let name = c.name.clone();
        let shown = self.browser_crate == Some(i);
        self.save_crates();
        if shown {
            self.browser_crate = self.crate_index(&name);
        }
        self.log(format!("{added} track{} added to \"{name}\"", if added == 1 { "" } else { "s" }));
    }

    pub fn remove_from_crate(&mut self, i: usize, path: &Path) {
        if let Some(c) = self.crates.get_mut(i) {
            c.tracks.retain(|p| p != path);
            let _ = crate::settings::save_json_later(&Self::crates_path(), &self.crates);
        }
    }

    /// Imported crates replace those imported before from the same place under the same name;
    /// yours stay. Then the library is scanned again for their tracks outside its folders.
    fn merge_imported(&mut self, from: &str, imported: Vec<dj_library::crates::Crate>) -> (usize, usize, usize) {
        let shown = self.browser_crate.and_then(|i| self.crates.get(i)).map(|c| c.name.clone());
        let n = imported.len();
        let tracks: usize = imported.iter().map(|c| c.tracks.len()).sum();
        let missing = imported.iter().flat_map(|c| &c.tracks).filter(|p| !p.exists()).count();
        for c in imported {
            let suffixed = format!("{} ({from})", c.name);
            match self.crates.iter().position(|o| o.source == c.source && (o.name == c.name || o.name == suffixed)) {
                // (Updated under the name it has here.)
                Some(j) => {
                    let name = std::mem::take(&mut self.crates[j].name);
                    self.crates[j] = dj_library::crates::Crate { name, ..c };
                }
                // (A crate of yours by that name: the import gets the source in its name.)
                None if self.crate_index(&c.name).is_some() => self.crates.push(dj_library::crates::Crate { name: suffixed, ..c }),
                None => self.crates.push(c),
            }
        }
        self.save_crates();
        self.browser_crate = shown.and_then(|n| self.crate_index(&n));
        self.rescan_library();
        (n, tracks, missing)
    }

    /// Import the crates of every `_Serato_` folder (the Music folder's and external drives').
    pub fn import_serato_crates(&mut self) {
        let dirs = dj_library::crates::serato_dirs();
        if dirs.is_empty() {
            self.log("No Serato crates found (no _Serato_/Subcrates in the Music folder or on a drive)");
            return;
        }
        let mut all = Vec::new();
        for d in &dirs {
            match dj_library::crates::import_serato(d) {
                Ok(c) => all.extend(c),
                Err(e) => self.log(format!("Serato crates: {e}")),
            }
        }
        let (n, t, missing) = self.merge_imported("Serato", all);
        let gone = if missing > 0 { format!(", {missing} files not found") } else { String::new() };
        self.log(format!("Serato: {n} crates, {t} tracks{gone}"));
    }

    /// Import the playlists of a rekordbox XML export (File → Export Collection in xml format).
    pub fn import_rekordbox(&mut self, path: &Path) {
        let read = std::fs::read_to_string(path).map_err(|e| e.to_string()).and_then(|x| dj_library::crates::read_rekordbox_xml(&x));
        match read {
            Ok(crates) => {
                let (n, t, missing) = self.merge_imported("rekordbox", crates);
                let gone = if missing > 0 { format!(", {missing} files not found") } else { String::new() };
                self.log(format!("rekordbox: {n} playlists, {t} tracks{gone}"));
            }
            Err(e) => self.log(format!("rekordbox import: {e}")),
        }
    }

    // ------------------------------------------------------------ channel / mixer

    pub fn set_channel(&mut self, ch: usize, f: impl FnOnce(&mut ChannelParams)) {
        if ch >= NUM_DECKS {
            return;
        }
        let p = &mut self.session.channels[ch].params;
        f(p);
        let p = *p;
        let params = self.engine_params(ch, p);
        self.send(Command::Channel { ch: ch as u8, params });
        self.mark_dirty();
    }

    // ------------------------------------------------------------ output routing

    /// Stereo output pairs of the audio device.
    pub fn output_pairs(&self) -> usize {
        (self.audio_channels / 2).max(1)
    }

    /// External-mixer mode is active (and the device has more than one stereo pair to use).
    pub fn external_routing(&self) -> bool {
        self.settings.external_mixer && self.output_pairs() >= 2
    }

    /// Output pair a channel uses in external-mixer mode: one pair per deck when there are enough,
    /// otherwise left decks (1, 3) share the first pair and right decks (2, 4) the second.
    pub fn external_pair(&self, ch: usize) -> usize {
        ch % self.output_pairs()
    }

    /// The params actually sent to the engine: in external-mixer mode the output is forced to the
    /// deck's own pair. The session keeps the user's own routing for when the mode is turned off.
    fn engine_params(&self, ch: usize, mut p: ChannelParams) -> ChannelParams {
        if self.external_routing() {
            p.output = Output::Pair(self.external_pair(ch) as u8);
        }
        p
    }

    /// Stages applied on directly routed channels: none when an external mixer does the mixing.
    fn ext_stages(&self) -> ExtStages {
        if self.external_routing() {
            ExtStages { trim: false, eq: false, filter: false, volume: false }
        } else {
            self.settings.ext_stages
        }
    }

    /// Turn external-mixer mode on or off and re-route every channel.
    pub fn set_external_mixer(&mut self, on: bool) {
        self.settings.external_mixer = on;
        self.send(Command::ExtStages(self.ext_stages()));
        for ch in 0..NUM_DECKS {
            let params = self.engine_params(ch, self.session.channels[ch].params);
            self.send(Command::Channel { ch: ch as u8, params });
        }
        self.mark_dirty();
        self.log(if self.external_routing() {
            format!("External mixer: decks routed to {} output pairs", self.output_pairs())
        } else if on {
            "External mixer: the audio device has only one stereo output, so decks are mixed internally".to_string()
        } else {
            "Internal mixer".to_string()
        });
    }

    // ------------------------------------------------------------ FX inserts (§7.4)

    fn first_free_slot(&self, ch: usize) -> Option<usize> {
        self.session.channels[ch].fx.slots.iter().position(|s| s.is_none())
    }

    /// Add an insert in the first free slot. PRE inserts are placed before
    /// POST ones in signal order regardless of slot index.
    pub fn add_insert(&mut self, ch: usize, effect_id: &str, pos: InsertPos) -> anyhow::Result<usize> {
        let slot = self.first_free_slot(ch).ok_or_else(|| anyhow::anyhow!("all {MAX_SLOTS} slots are in use"))?;
        self.put_insert(ch, slot, effect_id, pos, None)?;
        Ok(slot)
    }

    /// Replace a slot's effect (keeps PRE/POST and mix).
    pub fn replace_insert(&mut self, ch: usize, slot: usize, effect_id: &str) -> anyhow::Result<()> {
        let pos = self.session.channels[ch].fx.slots[slot].as_ref().map(|s| s.pos).unwrap_or_default();
        self.put_insert(ch, slot, effect_id, pos, None)
    }

    fn put_insert(&mut self, ch: usize, slot: usize, effect_id: &str, pos: InsertPos, restore: Option<&FxSlotModel>) -> anyhow::Result<()> {
        let (live, mut fx, name, vendor) = fxcat::instantiate(effect_id, &self.catalog_vst3, self.sample_rate, self.max_block)?;
        let mut model = FxSlotModel {
            effect_id: effect_id.to_string(),
            name,
            vendor,
            pos,
            on: true,
            mix: 1.0,
            params: live.defaults(),
            vst3_state: None,
            missing: false,
        };
        if let Some(r) = restore {
            model.on = r.on;
            model.mix = r.mix;
            if let (FxLive::Vst3(h), Some((c, e))) = (&live, &r.vst3_state) {
                h.load_state(&dj_vst3::State { component: fxcat::unb64(c), controller: fxcat::unb64(e) });
                model.params = live.defaults(); // re-read from the controller
                model.vst3_state = r.vst3_state.clone();
            } else if r.params.len() == model.params.len() {
                model.params = r.params.clone();
            }
        }
        for (i, v) in model.params.iter().enumerate() {
            fx.set_param(i, *v);
        }
        let (on, mix) = (model.on, model.mix);
        self.session.channels[ch].fx.slots[slot] = Some(model);
        self.fx_live[ch][slot] = Some(live);
        self.send(Command::FxInsert { ch: ch as u8, slot: slot as u8, effect: fx, pos, on, mix });
        self.mark_dirty();
        Ok(())
    }

    pub fn remove_insert(&mut self, ch: usize, slot: usize) {
        self.session.channels[ch].fx.slots[slot] = None;
        self.fx_live[ch][slot] = None;
        for m in self.session.channels[ch].fx.macros.iter_mut() {
            m.targets.retain(|t| t.slot as usize != slot);
        }
        self.send(Command::FxRemove { ch: ch as u8, slot: slot as u8 });
        self.resend_macros(ch);
        self.mark_dirty();
    }

    pub fn move_insert(&mut self, ch: usize, from: usize, to: usize) {
        if from == to || from >= MAX_SLOTS || to >= MAX_SLOTS {
            return;
        }
        let fx = &mut self.session.channels[ch].fx;
        let live = &mut self.fx_live[ch];
        if from < to {
            fx.slots[from..=to].rotate_left(1);
            live[from..=to].rotate_left(1);
        } else {
            fx.slots[to..=from].rotate_right(1);
            live[to..=from].rotate_right(1);
        }
        for m in fx.macros.iter_mut() {
            for t in m.targets.iter_mut() {
                let s = t.slot as usize;
                t.slot = if s == from {
                    to as u8
                } else if from < to && s > from && s <= to {
                    (s - 1) as u8
                } else if to < from && s >= to && s < from {
                    (s + 1) as u8
                } else {
                    t.slot
                };
            }
        }
        self.send(Command::FxMove { ch: ch as u8, from: from as u8, to: to as u8 });
        self.mark_dirty();
    }

    pub fn set_slot(&mut self, ch: usize, slot: usize, f: impl FnOnce(&mut FxSlotModel)) {
        if let Some(s) = self.session.channels[ch].fx.slots[slot].as_mut() {
            f(s);
            let (pos, on, mix) = (s.pos, s.on, s.mix);
            self.send(Command::FxSlot { ch: ch as u8, slot: slot as u8, pos, on, mix });
            self.mark_dirty();
        }
    }

    pub fn set_fx_param(&mut self, ch: usize, slot: usize, param: usize, value: f32) {
        let v = value.clamp(0.0, 1.0);
        if let Some(s) = self.session.channels[ch].fx.slots[slot].as_mut() {
            if let Some(p) = s.params.get_mut(param) {
                *p = v;
            }
        }
        if let Some(FxLive::Vst3(h)) = &self.fx_live[ch][slot] {
            h.set_param(param, v as f64);
        }
        self.send(Command::FxParam { ch: ch as u8, slot: slot as u8, param: param as u32, value: v });
        self.mark_dirty();
    }

    pub fn set_bypass_all(&mut self, ch: usize, on: bool) {
        self.session.channels[ch].fx.bypass_all = on;
        self.send(Command::FxBypassAll { ch: ch as u8, on });
        self.mark_dirty();
    }

    /// Move a macro: drives every assigned parameter (mirrored in the model).
    pub fn set_macro(&mut self, ch: usize, idx: usize, value: f32) {
        if idx >= NUM_MACROS {
            return;
        }
        let v = value.clamp(0.0, 1.0);
        let m = &mut self.session.channels[ch].fx.macros[idx];
        m.value = v;
        let targets = m.targets.clone();
        for t in targets {
            let pv = t.map(v);
            let (slot, param) = (t.slot as usize, t.param as usize);
            if let Some(s) = self.session.channels[ch].fx.slots[slot].as_mut() {
                if let Some(p) = s.params.get_mut(param) {
                    *p = pv;
                }
            }
            if let Some(FxLive::Vst3(h)) = &self.fx_live[ch][slot] {
                h.set_param(param, pv as f64);
            }
        }
        self.send(Command::FxMacro { ch: ch as u8, idx: idx as u8, value: v });
        self.mark_dirty();
    }

    pub fn assign_macro(&mut self, ch: usize, idx: usize, slot: usize, param: usize) {
        let cur = self.session.channels[ch].fx.slots[slot].as_ref().and_then(|s| s.params.get(param).copied()).unwrap_or(0.5);
        let m = &mut self.session.channels[ch].fx.macros[idx];
        m.targets.retain(|t| !(t.slot as usize == slot && t.param as usize == param));
        if m.targets.len() >= MACRO_TARGETS {
            m.targets.remove(0);
        }
        if m.targets.is_empty() {
            m.value = cur;
        }
        m.targets.push(MacroTarget { slot: slot as u8, param: param as u32, min: 0.0, max: 1.0 });
        self.resend_macros(ch);
        self.mark_dirty();
    }

    pub fn set_macro_target(&mut self, ch: usize, idx: usize, t: usize, f: impl FnOnce(&mut MacroTarget)) {
        if let Some(target) = self.session.channels[ch].fx.macros[idx].targets.get_mut(t) {
            f(target);
        }
        self.resend_macros(ch);
        self.mark_dirty();
    }

    pub fn clear_macro(&mut self, ch: usize, idx: usize, t: Option<usize>) {
        let m = &mut self.session.channels[ch].fx.macros[idx];
        match t {
            Some(t) if t < m.targets.len() => {
                m.targets.remove(t);
            }
            _ => m.targets.clear(),
        }
        self.resend_macros(ch);
        self.mark_dirty();
    }

    fn resend_macros(&mut self, ch: usize) {
        for i in 0..NUM_MACROS {
            let targets = self.session.channels[ch].fx.macros[i].engine_targets();
            self.send(Command::FxMacroTargets { ch: ch as u8, idx: i as u8, targets });
        }
    }

    pub fn rearm_insert(&mut self, ch: usize, slot: usize) {
        self.send(Command::FxRearm { ch: ch as u8, slot: slot as u8 });
    }

    /// Capture VST3 state into the model (before saving a preset/session).
    pub fn snapshot_plugin_states(&mut self) {
        for ch in 0..NUM_DECKS {
            for slot in 0..MAX_SLOTS {
                if let Some(FxLive::Vst3(h)) = &self.fx_live[ch][slot] {
                    let st = h.save_state();
                    if let Some(s) = self.session.channels[ch].fx.slots[slot].as_mut() {
                        s.vst3_state = Some((fxcat::b64(&st.component), fxcat::b64(&st.controller)));
                    }
                }
            }
        }
    }

    /// Replace a channel's whole chain (preset load, session restore).
    pub fn restore_chain(&mut self, ch: usize, chain: FxChainModel) {
        for slot in 0..MAX_SLOTS {
            if self.session.channels[ch].fx.slots[slot].is_some() || self.fx_live[ch][slot].is_some() {
                self.session.channels[ch].fx.slots[slot] = None;
                self.fx_live[ch][slot] = None;
                self.send(Command::FxRemove { ch: ch as u8, slot: slot as u8 });
            }
        }
        for (slot, s) in chain.slots.iter().enumerate() {
            if let Some(s) = s {
                if let Err(e) = self.put_insert(ch, slot, &s.effect_id.clone(), s.pos, Some(s)) {
                    self.log(format!("CH{} insert {}: {} — kept as missing", ch + 1, slot + 1, e));
                    let mut m = s.clone();
                    m.missing = true;
                    self.session.channels[ch].fx.slots[slot] = Some(m);
                }
            }
        }
        let fx = &mut self.session.channels[ch].fx;
        fx.macros = chain.macros.clone();
        fx.bypass_all = chain.bypass_all;
        fx.name = chain.name.clone();
        self.send(Command::FxBypassAll { ch: ch as u8, on: chain.bypass_all });
        self.resend_macros(ch);
        for (i, m) in chain.macros.iter().enumerate() {
            if !m.targets.is_empty() {
                self.send(Command::FxMacro { ch: ch as u8, idx: i as u8, value: m.value });
            }
        }
        self.mark_dirty();
    }

    pub fn save_fx_preset(&mut self, ch: usize, name: &str) {
        self.snapshot_plugin_states();
        self.session.channels[ch].fx.name = name.to_string();
        match self.session.channels[ch].fx.save_preset(name) {
            Ok(p) => self.log(format!("Saved FX preset {}", p.display())),
            Err(e) => self.log(format!("Could not save preset: {e}")),
        }
    }

    pub fn load_fx_preset(&mut self, ch: usize, path: &Path) {
        match FxChainModel::load_preset(path) {
            Some(c) => {
                self.restore_chain(ch, c);
                self.log(format!("Loaded FX preset into CH{}", ch + 1));
            }
            None => self.log(format!("Could not read preset {}", path.display())),
        }
    }

    // ------------------------------------------------------------ sampler

    pub fn set_deck_mode(&mut self, d: usize, mode: DeckMode) {
        self.session.decks[d].mode = mode;
        if mode == DeckMode::Sampler {
            self.decks[d].info = None;
            self.session.decks[d].track = None;
            self.send(Command::DeckMode { deck: d as u8, mode });
            self.sync_sampler(d);
        } else {
            self.send(Command::DeckMode { deck: d as u8, mode });
        }
        self.mark_dirty();
    }

    pub fn set_lane(&mut self, d: usize, lane: usize, f: impl FnOnce(&mut LaneParams)) {
        if lane >= LANES {
            return;
        }
        let p = &mut self.session.decks[d].sampler.lanes[lane].params;
        f(p);
        let params = *p;
        self.send(Command::Sampler { deck: d as u8, cmd: SamplerCmd::Lane { lane: lane as u8, params } });
        self.mark_dirty();
    }

    pub fn set_step(&mut self, d: usize, lane: usize, step: usize, value: Step) {
        let sm = &mut self.session.decks[d].sampler;
        let pattern = sm.pattern;
        if let Some(s) = sm.patterns.get_mut(pattern).and_then(|p| p.get_mut(lane)).and_then(|l| l.get_mut(step)) {
            *s = value;
        }
        self.send(Command::Sampler {
            deck: d as u8,
            cmd: SamplerCmd::Step { pattern: pattern as u8, lane: lane as u8, step: step as u8, value },
        });
        self.mark_dirty();
    }

    pub fn sampler_cmd(&mut self, d: usize, cmd: SamplerCmd) {
        let sm = &mut self.session.decks[d].sampler;
        match cmd {
            SamplerCmd::Pattern(p) => sm.pattern = p as usize,
            SamplerCmd::Swing(s) => sm.swing = s,
            SamplerCmd::GlobalPitch(p) => sm.global_pitch = p,
            SamplerCmd::Speed(s) => sm.speed = s,
            _ => {}
        }
        self.send(Command::Sampler { deck: d as u8, cmd });
        self.mark_dirty();
    }

    pub fn load_sample(&mut self, d: usize, lane: usize, path: &Path) {
        self.session.decks[d].sampler.lanes[lane].sample = Some(path.to_path_buf());
        let _ = self.loader_tx.send(LoadJob::Sample { deck: d, lane, path: path.to_path_buf() });
        self.mark_dirty();
    }

    pub fn clear_sample(&mut self, d: usize, lane: usize) {
        self.session.decks[d].sampler.lanes[lane].sample = None;
        self.send(Command::SamplerSample { deck: d as u8, lane: lane as u8, audio: None });
        self.mark_dirty();
    }

    // ------------------------------------------------------------ recording

    pub fn toggle_recording(&mut self) {
        if let Some(mut r) = self.recorder.take() {
            self.send(Command::StopRecording);
            let path = r.path.clone();
            if let Some(t) = r.thread.take() {
                std::thread::spawn(move || {
                    let _ = t.join();
                });
            }
            self.log(format!("Recording saved to {}", path.display()));
            return;
        }
        let dir = dirs::audio_dir().or_else(dirs::home_dir).unwrap_or_else(|| PathBuf::from(".")).join("dj Recordings");
        let _ = std::fs::create_dir_all(&dir);
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let path = dir.join(format!("dj-{stamp}.wav"));
        let (prod, mut cons) = rtrb::RingBuffer::<[f32; 2]>::new((self.sample_rate as usize) * 8);
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: self.sample_rate as u32,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let writer = match hound::WavWriter::create(&path, spec) {
            Ok(w) => w,
            Err(e) => {
                self.log(format!("Cannot record: {e}"));
                return;
            }
        };
        let thread = std::thread::spawn(move || {
            let mut w = writer;
            loop {
                let mut wrote = false;
                while let Ok(f) = cons.pop() {
                    let _ = w.write_sample(f[0]);
                    let _ = w.write_sample(f[1]);
                    wrote = true;
                }
                if !wrote {
                    if cons.is_abandoned() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
            let _ = w.finalize();
        });
        self.send(Command::StartRecording(prod));
        self.log(format!("Recording to {}", path.display()));
        self.recorder = Some(Recorder { path, started: Instant::now(), thread: Some(thread) });
    }

    // ------------------------------------------------------------ routing

    /// Strip the configured root ("/dj/", or "/Syndicate/" for
    /// compatibility) from an incoming address.
    pub fn strip_root<'a>(&self, addr: &'a str) -> Option<&'a str> {
        let root = self.settings.osc_root.trim_end_matches('/');
        let a = addr.trim_start_matches('/');
        let r = root.trim_start_matches('/');
        if a.len() > r.len() && a[..r.len()].eq_ignore_ascii_case(r) && a.as_bytes()[r.len()] == b'/' {
            Some(&a[r.len() + 1..])
        } else if r.is_empty() {
            Some(a)
        } else {
            None
        }
    }

    /// The control under the mouse / last moved, for MIDI learn.
    pub fn touch(&mut self, addr: impl Into<String>) {
        if self.learn {
            self.last_touched = Some(addr.into());
        }
    }

    /// Route a control message. `addr` is relative to the root, e.g.
    /// "Deck/1/PlayPause". An optional ":value" suffix supplies the value.
    pub fn route(&mut self, addr: &str, args: &[OscType]) -> bool {
        let (addr, forced): (&str, Option<Vec<OscType>>) = match addr.split_once(':') {
            Some((a, v)) => {
                let val = v.parse::<i32>().map(OscType::Int).or_else(|_| v.parse::<f32>().map(OscType::Float));
                match val {
                    Ok(x) => (a, Some(vec![x])),
                    Err(_) => (a, None),
                }
            }
            None => (addr, None),
        };
        let args = forced.as_deref().unwrap_or(args);
        let parts: Vec<&str> = addr.trim_matches('/').split('/').collect();
        let lower: Vec<String> = parts.iter().map(|p| p.to_ascii_lowercase()).collect();
        let l: Vec<&str> = lower.iter().map(|s| s.as_str()).collect();
        let f = arg_f(args);
        let on = f.map(|v| v >= 0.5);
        let trig = f.map(|v| v != 0.0).unwrap_or(true);
        let ints = arg_ints(args);
        match l.as_slice() {
            ["header", rest @ ..] => self.route_header(rest, f, on, trig, &ints),
            ["deck", n, "sampler", rest @ ..] => {
                let Some(d) = n.parse::<usize>().ok().and_then(|n| n.checked_sub(1)).filter(|d| *d < NUM_DECKS) else {
                    return false;
                };
                self.route_sampler(d, rest, f, on, trig, &ints, args)
            }
            ["deck", n, rest @ ..] => {
                let Some(d) = n.parse::<usize>().ok().and_then(|n| n.checked_sub(1)).filter(|d| *d < NUM_DECKS) else {
                    return false;
                };
                self.route_deck(d, rest, f, on, trig, &ints, args)
            }
            ["mixer", ch, rest @ ..] => {
                let Some(c) = ch.strip_prefix("ch").and_then(|n| n.parse::<usize>().ok()).and_then(|n| n.checked_sub(1)).filter(|c| *c < NUM_DECKS)
                else {
                    return false;
                };
                self.route_mixer(c, rest, f, on, &ints, args)
            }
            ["feedback", "resend"] if trig => {
                self.feedback.clear();
                true
            }
            ["browser", "browse"] => {
                if let Some(&n) = ints.first() {
                    let list = self.filtered_library();
                    if !list.is_empty() {
                        let cur = self.browser_selected.and_then(|s| list.iter().position(|&i| i == s)).unwrap_or(0) as i32;
                        let next = (cur + n).clamp(0, list.len() as i32 - 1) as usize;
                        self.browser_selected = Some(list[next]);
                    }
                }
                true
            }
            _ => false,
        }
    }

    fn route_header(&mut self, rest: &[&str], f: Option<f32>, on: Option<bool>, trig: bool, ints: &[i32]) -> bool {
        match rest {
            ["bpm"] => {
                if let Some(v) = f {
                    let bpm = (v as f64).clamp(20.0, 999.0);
                    self.session.bpm = bpm;
                    self.send(Command::SetBpm(bpm));
                }
            }
            ["bpmstep"] => {
                if let Some(&n) = ints.first() {
                    let bpm = (self.snap.clock.bpm + n as f64 * 0.1).clamp(20.0, 999.0);
                    self.session.bpm = bpm;
                    self.send(Command::SetBpm(bpm));
                }
            }
            ["bend"] => self.send(Command::HeaderBend(f.map(|v| v.signum() as f64 * 2.0).unwrap_or(0.0))),
            ["quantize"] => {
                if let Some(&i) = ints.first() {
                    // 0 none, 1 = 2 bars … 7 = 1/32 bar
                    if i <= 0 {
                        self.session.quantize_on = false;
                    } else {
                        self.session.quantize_on = true;
                        self.session.quantize = (i as usize - 1).min(QUANTIZE_VALUES.len() - 1);
                    }
                    self.send(Command::GlobalQuantize(self.session.global_quantize()));
                }
            }
            ["quantizeon"] => {
                if let Some(o) = on {
                    self.session.quantize_on = o;
                    self.send(Command::GlobalQuantize(self.session.global_quantize()));
                }
            }
            ["mastervolume"] => {
                if let Some(v) = f {
                    self.session.master.volume = v.clamp(0.0, 1.0);
                    self.apply_master();
                }
            }
            ["cuevolume"] => {
                if let Some(v) = f {
                    self.session.master.cue_volume = v.clamp(0.0, 1.0);
                    self.apply_master();
                }
            }
            ["cuemix"] => {
                if let Some(v) = f {
                    self.session.master.cue_mix = v.clamp(0.0, 1.0);
                    self.apply_master();
                }
            }
            ["limiter"] => {
                if let Some(o) = on {
                    self.session.master.limiter = o;
                    self.apply_master();
                }
            }
            ["master"] if trig => self.send(Command::HeaderMaster),
            ["wallclock"] => {
                if let Some(o) = on {
                    self.session.wallclock = o;
                    self.send(Command::Wallclock(o));
                }
            }
            ["record"] => {
                if let Some(o) = on {
                    if o != self.recorder.is_some() {
                        self.toggle_recording();
                    }
                }
            }
            _ => return false,
        }
        self.mark_dirty();
        true
    }

    /// A deck's tempo fader moved to `fader` (−1..1). A synced deck that isn't the master plays at
    /// the clock's tempo, so its fader sets the clock: the master (or the header tempo) moves so
    /// this deck plays at the speed the fader asks for, and every synced deck follows. Its reset
    /// (0) is this track at its own tempo.
    fn tempo_cmd(&mut self, d: usize, fader: f64) -> Option<DeckCmd> {
        let v = self.snap.decks[d];
        let follower = v.sync && self.snap.clock.owner != ClockOwner::Deck(d as u8) && v.has_grid && v.track_bpm > 0.0;
        if !follower {
            return Some(DeckCmd::Tempo(fader));
        }
        let bpm = v.track_bpm * v.tempo_range.speed(fader) * v.speed_mult;
        match self.snap.clock.owner {
            ClockOwner::Deck(m) => {
                let mv = self.snap.decks[m as usize];
                if mv.track_bpm > 0.0 {
                    let mf = mv.tempo_range.fader_for_speed(bpm / (mv.track_bpm * mv.speed_mult.max(0.01)));
                    self.deck_cmd(m as usize, DeckCmd::Tempo(mf.clamp(-1.0, 1.0)));
                }
            }
            ClockOwner::Header => self.send(Command::SetBpm(bpm)),
        }
        // Its own fader goes along, so it reads the same once it leads or leaves sync.
        Some(DeckCmd::Tempo(fader))
    }

    fn route_deck(&mut self, d: usize, rest: &[&str], f: Option<f32>, on: Option<bool>, trig: bool, ints: &[i32], args: &[OscType]) -> bool {
        let pressed = on.unwrap_or(true);
        let skip = self.settings.skip_beats;
        let dm = self.session.decks[d].clone();
        let slip_mult = if dm.slip_dotted {
            1.5
        } else if dm.slip_triplet {
            2.0 / 3.0
        } else {
            1.0
        };
        let cmd: Option<DeckCmd> = match rest {
            ["playpause"] if trig => Some(DeckCmd::TogglePlay),
            ["playing"] => on.map(DeckCmd::SetPlaying),
            ["cue"] => {
                if pressed {
                    self.decks[d].cue_chosen = true;
                }
                Some(DeckCmd::Cue(pressed))
            }
            ["gridstart"] if trig => Some(DeckCmd::GridStart),
            // A hot cue pad (screen, MIDI, keys): see `pad_press`.
            [c] if c.starts_with("cue") && c.len() == 4 => {
                let i = c[3..].parse::<u8>().ok().filter(|i| (1..=8).contains(i));
                match (i, trig) {
                    (Some(i), true) => self.pad_press(d, i - 1),
                    _ => None,
                }
            }
            ["hotcue"] => match ints {
                [i] if (1..=8).contains(i) => self.pad_press(d, (*i - 1) as u8),
                // Press and release (momentary cues): the plain hot cue.
                [i, p, ..] if (1..=8).contains(i) => Some(DeckCmd::HotCue { idx: (*i - 1) as u8, pressed: *p != 0 }),
                _ => None,
            },
            ["loopsize"] => f.filter(|b| *b > 0.0).map(|b| {
                self.loop_size[d] = (b as f64).clamp(1.0 / 32.0, 64.0);
                DeckCmd::DefaultLoopBeats(self.loop_size[d])
            }),
            ["setcueloop"] => ints.first().filter(|i| (1..=8).contains(*i)).map(|i| DeckCmd::CueLoop((*i - 1) as u8)),
            // Cue number, then the loop length in beats (default: the deck's loop size).
            ["cuebeatloop"] => match ints.first() {
                Some(i) if (1..=8).contains(i) => {
                    let beats = args.get(1).and_then(|a| arg_f(std::slice::from_ref(a))).filter(|b| *b > 0.0);
                    Some(DeckCmd::CueBeatLoop {
                        idx: (*i - 1) as u8,
                        beats: beats.map_or(self.loop_size[d], |b| b as f64),
                    })
                }
                _ => None,
            },
            ["deletecue"] => ints.first().filter(|i| (1..=8).contains(*i)).map(|i| DeckCmd::DeleteCue((*i - 1) as u8)),
            ["cuemomentary"] => on.map(|o| {
                self.session.decks[d].momentary_cues = o;
                DeckCmd::MomentaryCues(o)
            }),
            ["cueslip"] | ["slip"] => on.map(|o| {
                self.session.decks[d].slip = o;
                DeckCmd::Slip(o)
            }),
            ["temposync"] if trig => Some(DeckCmd::ToggleSync),
            ["sync"] => on.map(DeckCmd::Sync),
            ["tempomaster"] if trig => Some(DeckCmd::ToggleMaster),
            ["temposlider"] => f.and_then(|v| self.tempo_cmd(d, v.clamp(-1.0, 1.0) as f64)),
            ["tempo"] => f.and_then(|v| self.tempo_cmd(d, (v.clamp(0.0, 1.0) * 2.0 - 1.0) as f64)),
            ["temporange"] => ints.first().map(|i| {
                let r = TempoRange::ALL[(*i).clamp(0, 4) as usize];
                self.session.decks[d].tempo_range = r;
                DeckCmd::TempoRange(r)
            }),
            ["faderstart"] => {
                if let Some(o) = on {
                    self.session.channels[d].fader_start = o;
                    self.send(Command::FaderStart { ch: d as u8, on: o });
                }
                None
            }
            ["speed"] => ints.first().map(|i| DeckCmd::SpeedMult([0.5, 1.0, 2.0][(*i).clamp(0, 2) as usize])),
            ["speedhalf"] => on.map(|o| DeckCmd::SpeedMult(if o { 0.5 } else { 1.0 })),
            ["speeddouble"] => on.map(|o| DeckCmd::SpeedMult(if o { 2.0 } else { 1.0 })),
            ["reverse"] => on.map(DeckCmd::Reverse),
            ["backspin"] => Some(DeckCmd::Backspin(pressed)),
            ["quantize"] => ints.first().map(|i| {
                let q = Quantize::from_index(*i);
                self.session.decks[d].quantize = q;
                DeckCmd::Quantize(q)
            }),
            ["loop"] if trig => Some(DeckCmd::LoopToggle),
            ["loopin"] if trig => Some(DeckCmd::LoopIn),
            // One button for both ends: In, then Out; while a loop plays it exits.
            ["loopinout"] if trig => {
                let pending = std::mem::take(&mut self.decks[d].loop_in_pending);
                Some(if self.snap.decks[d].loop_on {
                    DeckCmd::LoopExit
                } else if pending {
                    DeckCmd::LoopOut
                } else {
                    self.decks[d].loop_in_pending = true;
                    DeckCmd::LoopIn
                })
            }
            ["loopout"] if trig => Some(DeckCmd::LoopOut),
            ["loopexit"] if trig => Some(DeckCmd::LoopExit),
            ["loopbeats"] => f.filter(|v| *v > 0.0).map(|v| DeckCmd::LoopBeats(v as f64)),
            // An encoder on the loop size: each step doubles or halves it (and a playing loop).
            ["loopsizestep"] => {
                let n = ints.first().copied().unwrap_or(0).clamp(-6, 6);
                if n == 0 {
                    return true;
                }
                let k = 2f64.powi(n);
                self.loop_size[d] = (self.loop_size[d] * k).clamp(1.0 / 32.0, 64.0);
                self.deck_cmd(d, DeckCmd::DefaultLoopBeats(self.loop_size[d]));
                self.snap.decks[d].loop_on.then_some(DeckCmd::LoopScale(k))
            }
            ["loophalve"] if trig => Some(DeckCmd::LoopScale(0.5)),
            ["loopdouble"] if trig => Some(DeckCmd::LoopScale(2.0)),
            ["freeloop"] => f.map(|v| DeckCmd::FreeLoop(5.0 + (v.clamp(0.0, 1.0) as f64).powi(2) * 995.0)),
            ["sliploop", size] => SLIP_LOOPS
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(size))
                .map(|(_, b)| DeckCmd::SlipLoop { beats: b * slip_mult, pressed }),
            ["slipdotted"] => {
                if let Some(o) = on {
                    self.session.decks[d].slip_dotted = o;
                    if o {
                        self.session.decks[d].slip_triplet = false;
                    }
                }
                None
            }
            ["sliptriplet"] => {
                if let Some(o) = on {
                    self.session.decks[d].slip_triplet = o;
                    if o {
                        self.session.decks[d].slip_dotted = false;
                    }
                }
                None
            }
            ["skipbeatback"] if trig => Some(DeckCmd::Skip(-skip)),
            ["skipbeatforward"] if trig => Some(DeckCmd::Skip(skip)),
            ["skipbarback"] if trig => Some(DeckCmd::Skip(-4.0)),
            ["skipbarforward"] if trig => Some(DeckCmd::Skip(4.0)),
            ["beatskip"] => ints.first().filter(|n| **n != 0).map(|n| DeckCmd::Skip(*n as f64)),
            ["barskip"] => ints.first().filter(|n| **n != 0).map(|n| DeckCmd::Skip(*n as f64 * 4.0)),
            ["loopmovebeat"] => ints.first().filter(|n| **n != 0).map(|n| DeckCmd::LoopMove(*n as f64)),
            ["loopmovebar"] => ints.first().filter(|n| **n != 0).map(|n| DeckCmd::LoopMove(*n as f64 * 4.0)),
            ["pitchpreserve"] => on.map(|o| {
                self.session.decks[d].keylock = o;
                DeckCmd::KeyLock(o)
            }),
            [s] if stem_switch(s).is_some() => on.map(|o| DeckCmd::StemGain {
                stem: stem_switch(s).unwrap() as u8,
                gain: if o { 1.0 } else { 0.0 },
            }),
            [s] if stem_level(s).is_some() => f.map(|v| DeckCmd::StemGain {
                stem: stem_level(s).unwrap() as u8,
                gain: v.clamp(0.0, 1.0),
            }),
            ["acapella"] => on.and_then(|o| self.set_stems(d, if o { STEMS_ACAPELLA } else { [1.0; 4] })),
            ["instrumental"] => on.and_then(|o| self.set_stems(d, if o { STEMS_INSTRUMENTAL } else { [1.0; 4] })),
            ["pitchdown"] if trig => Some(DeckCmd::Semitones(self.snap.decks[d].semitones - 1.0)),
            ["pitchup"] if trig => Some(DeckCmd::Semitones(self.snap.decks[d].semitones + 1.0)),
            ["pitchreset"] if trig => Some(DeckCmd::Semitones(0.0)),
            ["pitchstep"] => ints.first().map(|n| DeckCmd::Semitones(self.snap.decks[d].semitones + *n as f64)),
            ["pitch"] => f.map(|v| DeckCmd::Semitones((v as f64).round())),
            ["phaseminus"] => Some(DeckCmd::PhaseBend(if pressed { -1.0 } else { 0.0 })),
            ["phaseplus"] => Some(DeckCmd::PhaseBend(if pressed { 1.0 } else { 0.0 })),
            ["phasebend"] => f.map(|v| DeckCmd::PhaseBend(v.signum() as f64 * (v != 0.0) as i32 as f64)),
            ["phasereset"] if trig => Some(DeckCmd::PhaseReset),
            ["jog"] => {
                if let Some(&n) = ints.first() {
                    self.jog(d, n);
                }
                None
            }
            ["bend"] => {
                // Tempo bend on the master deck (±2 BPM while held).
                f.map(|v| DeckCmd::BpmBend(v.signum() as f64 * 2.0 * (v != 0.0) as i32 as f64))
            }
            ["waveformseek"] | ["seek"] => f.map(|v| DeckCmd::SeekFraction(v.clamp(0.0, 1.0) as f64)),
            ["waveformzoomin"] if trig => {
                self.session.decks[d].zoom = (self.session.decks[d].zoom * 1.5).min(16.0);
                None
            }
            ["waveformzoomout"] if trig => {
                self.session.decks[d].zoom = (self.session.decks[d].zoom / 1.5).max(0.25);
                None
            }
            ["waveformzoom"] => {
                if let Some(&n) = ints.first() {
                    self.session.decks[d].zoom = (self.session.decks[d].zoom * 1.5f32.powi(n)).clamp(0.25, 16.0);
                }
                None
            }
            ["eject"] if trig => {
                self.eject(d);
                None
            }
            // Load a file: `Deck/N/Load s /path/to/track.mp3`.
            ["load"] => {
                if let Some(OscType::String(path)) = args.first() {
                    self.request_load(d, Path::new(path));
                }
                None
            }
            ["loadselected"] if trig => {
                if let Some(e) = self.browser_selected.and_then(|i| self.library.get(i)).map(|e| e.path.clone()) {
                    self.request_load(d, &e);
                }
                None
            }
            ["gridstarthere"] if trig => {
                self.grid_start_here(d);
                None
            }
            ["gridnudge"] => {
                if let Some(v) = f {
                    self.nudge_grid(d, v as f64);
                }
                None
            }
            _ => return false,
        };
        if let Some(c) = cmd {
            self.deck_cmd(d, c);
        }
        self.mark_dirty();
        true
    }

    #[allow(clippy::too_many_arguments)]
    fn route_sampler(
        &mut self,
        d: usize,
        rest: &[&str],
        f: Option<f32>,
        on: Option<bool>,
        trig: bool,
        ints: &[i32],
        args: &[OscType],
    ) -> bool {
        let sel = self.session.decks[d].sampler.selected_lane;
        match rest {
            ["playpause"] if trig => self.sampler_cmd(d, SamplerCmd::Toggle),
            ["lane"] => {
                if let Some(&i) = ints.first() {
                    self.session.decks[d].sampler.selected_lane = (i.max(0) as usize).min(LANES - 1);
                }
            }
            ["trigger"] => {
                if let (Some(&lane), true) = (ints.first(), on.unwrap_or(true)) {
                    if (1..=LANES as i32).contains(&lane) {
                        self.send(Command::Sampler { deck: d as u8, cmd: SamplerCmd::Trigger { lane: (lane - 1) as u8, velocity: 1.0 } });
                    }
                }
            }
            ["pattern"] => {
                if let Some(&p) = ints.first() {
                    self.sampler_cmd(d, SamplerCmd::Pattern(p.clamp(0, 7) as u8));
                }
            }
            ["attack"] | ["hold"] | ["release"] | ["volume"] | ["curve"] | ["pitch"] | ["steps"] => {
                if let Some(v) = f {
                    self.set_lane_control(d, sel, rest[0], v);
                }
            }
            ["swing"] => {
                if let Some(v) = f {
                    self.sampler_cmd(d, SamplerCmd::Swing(v.clamp(0.0, 1.0)));
                }
            }
            ["globalpitch"] => {
                if let Some(v) = f {
                    self.sampler_cmd(d, SamplerCmd::GlobalPitch(v.clamp(-12.0, 12.0)));
                }
            }
            ["speed"] => {
                if let Some(&i) = ints.first() {
                    self.sampler_cmd(d, SamplerCmd::Speed([0.5, 1.0, 2.0][i.clamp(0, 2) as usize]));
                }
            }
            ["speedhalf"] => {
                if let Some(o) = on {
                    self.sampler_cmd(d, SamplerCmd::Speed(if o { 0.5 } else { 1.0 }));
                }
            }
            ["speeddouble"] => {
                if let Some(o) = on {
                    self.sampler_cmd(d, SamplerCmd::Speed(if o { 2.0 } else { 1.0 }));
                }
            }
            ["laneset"] => {
                let lane = args.first().and_then(|a| if let OscType::Int(i) = a { Some(*i) } else { None });
                let ctl = args.get(1).and_then(|a| if let OscType::String(s) = a { Some(s.to_ascii_lowercase()) } else { None });
                let val = args.get(2).and_then(|a| arg_f(std::slice::from_ref(a)));
                if let (Some(l), Some(c), Some(v)) = (lane, ctl, val) {
                    if (1..=LANES as i32).contains(&l) {
                        self.set_lane_control(d, (l - 1) as usize, &c, v);
                    }
                }
            }
            _ => return false,
        }
        true
    }

    fn set_lane_control(&mut self, d: usize, lane: usize, ctl: &str, v: f32) {
        use dj_engine::sampler::{Direction, Rhythm};
        self.set_lane(d, lane, |p| match ctl {
            "attack" => p.attack = v.clamp(0.0, 1.0),
            "hold" => p.hold = v.clamp(0.0, 1.0),
            "release" => p.release = v.clamp(0.0, 1.0),
            "curve" => p.curve = v.clamp(0.0, 1.0),
            "volume" => p.volume = v.clamp(0.0, 1.0),
            "pitch" => p.pitch = v.clamp(-24.0, 24.0),
            "steps" => p.steps = v.round().clamp(1.0, STEPS as f32) as u8,
            "mute" => p.mute = v != 0.0,
            "mono" => p.mono = v != 0.0,
            "direction" => {
                p.direction = match p.direction {
                    Direction::Forward => Direction::Reverse,
                    Direction::Reverse => Direction::PingPong,
                    Direction::PingPong => Direction::Random,
                    Direction::Random => Direction::Forward,
                }
            }
            "rhythm" => {
                p.rhythm = match p.rhythm {
                    Rhythm::Normal => Rhythm::Dotted,
                    Rhythm::Dotted => Rhythm::Triplet,
                    Rhythm::Triplet => Rhythm::Normal,
                }
            }
            _ => {}
        });
    }

    fn route_mixer(&mut self, c: usize, rest: &[&str], f: Option<f32>, on: Option<bool>, ints: &[i32], args: &[OscType]) -> bool {
        let strings: Vec<String> = args.iter().filter_map(|a| if let OscType::String(s) = a { Some(s.clone()) } else { None }).collect();
        match rest {
            // Extension: load an insert by catalog id, e.g. "native:echo" or a VST3 id.
            ["fx", "insert"] => {
                if let Some(id) = strings.first() {
                    let pos = if strings.get(1).map(|p| p.eq_ignore_ascii_case("post")).unwrap_or(false) { InsertPos::Post } else { InsertPos::Pre };
                    if let Err(e) = self.add_insert(c, id, pos) {
                        self.log(format!("CH{}: {e}", c + 1));
                    }
                }
            }
            ["fx", "slot", n, "remove"] => {
                if let Some(s) = n.parse::<usize>().ok().and_then(|n| n.checked_sub(1)).filter(|s| *s < MAX_SLOTS) {
                    self.remove_insert(c, s);
                }
            }
            ["volume"] => {
                if let Some(v) = f {
                    self.set_channel(c, |p| p.volume = v.clamp(0.0, 1.0));
                }
            }
            ["trim"] => {
                if let Some(v) = f {
                    self.set_channel(c, |p| p.trim_db = (v.clamp(0.0, 1.0) - 0.5) * 12.0);
                }
            }
            ["eq", band] => {
                let b = match *band {
                    "low" => 0,
                    "mid" => 1,
                    "high" | "hi" => 2,
                    _ => return false,
                };
                if let Some(v) = f {
                    self.set_channel(c, |p| p.eq[b] = v.clamp(0.0, 1.0));
                }
            }
            ["filter1"] => {
                if let Some(v) = f {
                    self.set_channel(c, |p| p.cutoff = v.clamp(0.0, 1.0));
                }
            }
            ["filter2"] => {
                if let Some(v) = f {
                    self.set_channel(c, |p| p.res = v.clamp(0.0, 1.0));
                }
            }
            ["filter3"] => {
                if let Some(v) = f {
                    self.set_channel(c, |p| p.width = v.clamp(0.0, 1.0));
                }
            }
            ["mute"] => {
                if let Some(o) = on {
                    self.set_channel(c, |p| p.mute = o);
                }
            }
            ["solo"] => {
                if let Some(o) = on {
                    self.set_channel(c, |p| p.solo = o);
                }
            }
            ["cue"] => {
                if let Some(o) = on {
                    self.set_channel(c, |p| p.cue = o);
                }
            }
            ["faderstart"] => {
                if let Some(o) = on {
                    self.session.channels[c].fader_start = o;
                    self.send(Command::FaderStart { ch: c as u8, on: o });
                }
            }
            ["output"] => {
                if let Some(&i) = ints.first() {
                    self.set_channel(c, |p| p.output = if i <= 0 { Output::Master } else { Output::Pair((i - 1) as u8) });
                }
            }
            // §7.4 routes: FX switch (1 runs inserts, 0 bypasses all) and macros.
            ["fx"] => {
                if let Some(o) = on {
                    self.set_bypass_all(c, !o);
                }
            }
            ["fx", "macro", n] => {
                let i = n.parse::<usize>().ok().and_then(|n| n.checked_sub(1)).filter(|i| *i < NUM_MACROS);
                if let (Some(i), Some(v)) = (i, f) {
                    self.set_macro(c, i, v);
                }
            }
            // Extensions: per-slot control.
            ["fx", "slot", n, what, rest @ ..] => {
                let Some(s) = n.parse::<usize>().ok().and_then(|n| n.checked_sub(1)).filter(|s| *s < MAX_SLOTS) else {
                    return false;
                };
                match (*what, rest) {
                    ("on", []) => {
                        if let Some(o) = on {
                            self.set_slot(c, s, |m| m.on = o);
                        }
                    }
                    ("mix", []) => {
                        if let Some(v) = f {
                            self.set_slot(c, s, |m| m.mix = v.clamp(0.0, 1.0));
                        }
                    }
                    ("param", [k]) => {
                        if let (Some(k), Some(v)) = (k.parse::<usize>().ok().and_then(|k| k.checked_sub(1)), f) {
                            self.set_fx_param(c, s, k, v);
                        }
                    }
                    _ => return false,
                }
            }
            // Old native-FX routes from the original are accepted and ignored.
            ["reverb", ..] | ["delay", ..] | ["echo", ..] => {}
            _ => return false,
        }
        true
    }

    /// Current state of a switch address (for MIDI toggle mode & LEDs).
    pub fn switch_state(&self, addr: &str) -> Option<bool> {
        let parts: Vec<String> = addr.split(':').next()?.trim_matches('/').split('/').map(|s| s.to_ascii_lowercase()).collect();
        let p: Vec<&str> = parts.iter().map(|s| s.as_str()).collect();
        let idx = |s: &str| s.trim_start_matches("ch").parse::<usize>().ok().and_then(|n| n.checked_sub(1)).filter(|i| *i < NUM_DECKS);
        Some(match p.as_slice() {
            ["deck", n, "playpause"] | ["deck", n, "playing"] => self.snap.decks[idx(n)?].playing,
            ["deck", n, "temposync"] | ["deck", n, "sync"] => self.snap.decks[idx(n)?].sync,
            ["deck", n, "tempomaster"] => self.snap.clock.owner == ClockOwner::Deck(idx(n)? as u8),
            ["deck", n, "loop"] => self.snap.decks[idx(n)?].loop_on,
            ["deck", n, "pitchpreserve"] => self.snap.decks[idx(n)?].keylock,
            ["deck", n, s] if stem_switch(s).is_some() => self.snap.decks[idx(n)?].stem_gains[stem_switch(s)?] > 0.5,
            ["deck", n, "acapella"] => self.snap.decks[idx(n)?].stem_gains == STEMS_ACAPELLA,
            ["deck", n, "instrumental"] => self.snap.decks[idx(n)?].stem_gains == STEMS_INSTRUMENTAL,
            ["deck", n, "cueslip"] | ["deck", n, "slip"] => self.snap.decks[idx(n)?].slip,
            ["deck", n, "reverse"] => self.snap.decks[idx(n)?].reverse,
            ["deck", n, "cuemomentary"] => self.snap.decks[idx(n)?].momentary_cues,
            ["deck", n, c] if c.starts_with("cue") && c.len() == 4 => {
                let d = idx(n)?;
                let i = c[3..].parse::<usize>().ok()?.checked_sub(1)?;
                self.snap.decks[d].hot_cues.get(i)?.is_some()
            }
            ["deck", n, "sampler", "playpause"] => self.snap.samplers[idx(n)?].playing,
            ["mixer", c, "mute"] => self.session.channels[idx(c)?].params.mute,
            ["mixer", c, "solo"] => self.session.channels[idx(c)?].params.solo,
            ["mixer", c, "cue"] => self.session.channels[idx(c)?].params.cue,
            ["mixer", c, "fx"] => !self.session.channels[idx(c)?].fx.bypass_all,
            ["mixer", c, "faderstart"] => self.session.channels[idx(c)?].fader_start,
            ["header", "limiter"] => self.session.master.limiter,
            ["header", "wallclock"] => self.session.wallclock,
            ["header", "quantizeon"] => self.session.quantize_on,
            ["header", "master"] => self.snap.clock.owner == ClockOwner::Header,
            ["header", "record"] => self.recorder.is_some(),
            _ => return None,
        })
    }

    // ------------------------------------------------------------ MIDI input

    /// Whether CC `n` (32–63) on `ch` is the fine half of a 14-bit control: CC `n - 32` is mapped
    /// and `n` itself isn't (a controller that uses 32–63 as plain controls keeps them).
    fn is_fine_half(&self, ch: u8, n: u8) -> bool {
        let src = |number: u8| MidiSource { kind: MidiKind::Cc, channel: ch, number };
        let mapped = |number: u8| self.profile.mappings.iter().any(|m| m.source == src(number));
        self.cc_fine[ch as usize][(n - 32) as usize] || (mapped(n - 32) && !mapped(n))
    }

    /// A jog wheel turned `ticks` (relative; about 720 a turn): playing, it nudges the track
    /// (faster or slower while the wheel turns, like a turntable's platter edge); paused, it moves
    /// through the track, about 4 beats a turn.
    pub fn jog(&mut self, d: usize, ticks: i32) {
        if ticks == 0 || d >= NUM_DECKS {
            return;
        }
        if self.snap.decks[d].playing {
            if self.decks[d].jog_until.is_none() {
                self.deck_cmd(d, DeckCmd::PhaseBend(ticks.signum() as f64));
            }
            self.decks[d].jog_until = Some(Instant::now() + Duration::from_millis(JOG_HOLD_MS));
        } else {
            self.deck_cmd(d, DeckCmd::Skip(ticks as f64 / JOG_TICKS_PER_BEAT));
        }
    }

    /// The jog nudge ends once the wheel has stopped.
    fn jog_tick(&mut self) {
        for d in 0..NUM_DECKS {
            if self.decks[d].jog_until.is_some_and(|t| Instant::now() >= t) {
                self.decks[d].jog_until = None;
                self.deck_cmd(d, DeckCmd::PhaseBend(0.0));
            }
        }
    }

    pub(crate) fn handle_midi(&mut self, msg: &[u8]) {
        if msg.is_empty() {
            return;
        }
        let status = msg[0] & 0xf0;
        let ch = msg[0] & 0x0f;
        let (src, value) = match (status, msg.get(1), msg.get(2)) {
            (0x90, Some(&n), Some(&v)) => (MidiSource { kind: MidiKind::Note, channel: ch, number: n }, v as f32 / 127.0),
            (0x80, Some(&n), _) => (MidiSource { kind: MidiKind::Note, channel: ch, number: n }, 0.0),
            (0xb0, Some(&n), Some(&v)) => {
                let (c, n7) = (ch as usize, n as usize);
                if n7 < 32 {
                    self.cc_coarse[c][n7] = v;
                    if self.cc_fine[c][n7] {
                        return; // a 14-bit control: it moves when its fine half arrives
                    }
                    (MidiSource { kind: MidiKind::Cc, channel: ch, number: n }, v as f32 / 127.0)
                } else if (32..64).contains(&n7) && self.is_fine_half(ch, n) {
                    // The fine half of a 14-bit control: the full value, on the coarse CC's mapping.
                    let coarse = n7 - 32;
                    self.cc_fine[c][coarse] = true;
                    let full = ((self.cc_coarse[c][coarse] as u32) << 7 | v as u32) as f32 / 16383.0;
                    (MidiSource { kind: MidiKind::Cc, channel: ch, number: coarse as u8 }, full)
                } else {
                    (MidiSource { kind: MidiKind::Cc, channel: ch, number: n }, v as f32 / 127.0)
                }
            }
            (0xe0, Some(&lsb), Some(&msb)) => {
                (MidiSource { kind: MidiKind::PitchBend, channel: ch, number: 0 }, ((msb as u32) << 7 | lsb as u32) as f32 / 16383.0)
            }
            _ => return,
        };
        let raw = msg.get(2).copied().unwrap_or(0);

        if self.learn {
            if let Some(addr) = self.last_touched.clone() {
                if src.kind == MidiKind::Note && value == 0.0 {
                    return; // wait for a press
                }
                let kind = addr_kind(&addr);
                let mode = match (src.kind, kind) {
                    (MidiKind::Note, Kind::Switch) => MapMode::Toggle,
                    (MidiKind::Cc, Kind::Step) => MapMode::Relative,
                    _ => MapMode::Absolute,
                };
                self.profile.mappings.retain(|m| m.source != src);
                self.profile.mappings.push(MidiMapping { source: src, address: addr.clone(), mode, invert: false, feedback: true });
                let _ = self.profile.save();
                self.log(format!("Mapped {:?} ch{} #{} → {addr}", src.kind, src.channel + 1, src.number));
                self.last_touched = None;
                return;
            }
        }

        let maps: Vec<MidiMapping> = self.profile.mappings.iter().filter(|m| m.source == src).cloned().collect();
        for m in maps {
            self.midi_touched = Some((m.address.clone(), std::time::Instant::now()));
            let kind = addr_kind(&m.address);
            let v = if m.invert { 1.0 - value } else { value };
            match (m.mode, kind) {
                (MapMode::Toggle, _) => {
                    if v > 0.5 {
                        let cur = self.switch_state(&m.address).unwrap_or(false);
                        self.route(&m.address, &[OscType::Int(!cur as i32)]);
                    }
                }
                (MapMode::Relative | MapMode::RelativeCentred, _) => {
                    let step = match m.mode {
                        MapMode::RelativeCentred => raw as i32 - 64,
                        _ if raw < 64 => raw as i32,
                        _ => raw as i32 - 128,
                    };
                    if kind == Kind::Value {
                        // Relative encoder on a knob: nudge the current value.
                        let cur = self.value_state(&m.address).unwrap_or(0.5);
                        self.route(&m.address, &[OscType::Float((cur + step as f32 / 128.0).clamp(0.0, 1.0))]);
                    } else {
                        self.route(&m.address, &[OscType::Int(step)]);
                    }
                }
                (MapMode::Absolute, Kind::Trigger) => {
                    if v > 0.0 {
                        self.route(&m.address, &[OscType::Int(1)]);
                    }
                }
                (MapMode::Absolute, Kind::Switch | Kind::Hold) => {
                    self.route(&m.address, &[OscType::Int((v > 0.0) as i32)]);
                }
                (MapMode::Absolute, Kind::Step) => {
                    if v > 0.0 {
                        self.route(&m.address, &[OscType::Int(1)]);
                    }
                }
                (MapMode::Absolute, Kind::Bend) => {
                    self.route(&m.address, &[OscType::Int(if v > 0.55 { 1 } else if v < 0.45 && src.kind != MidiKind::Note { -1 } else { 0 })]);
                }
                (MapMode::Absolute, Kind::Index) => {
                    self.route(&m.address, &[OscType::Int((v * 127.0).round() as i32)]);
                }
                (MapMode::Absolute, Kind::Value) => {
                    let lower = m.address.to_ascii_lowercase();
                    // Tempo from a controller moves in the same steps as on screen.
                    let tempo_deck = lower
                        .trim_matches('/')
                        .strip_prefix("deck/")
                        .and_then(|r| r.split_once('/'))
                        .filter(|(_, c)| *c == "tempo" || *c == "temposlider")
                        .and_then(|(n, _)| n.parse::<usize>().ok()?.checked_sub(1))
                        .filter(|d| *d < NUM_DECKS);
                    let v = match tempo_deck {
                        Some(d) => snap_tempo(v, self.snap.decks[d].tempo_range, self.settings.tempo_step_pct),
                        None => v,
                    };
                    let scaled = if lower.ends_with("temposlider") || lower.ends_with("phasebend") {
                        v * 2.0 - 1.0
                    } else if lower.ends_with("/pitch") {
                        (v * 48.0 - 24.0).round()
                    } else if lower.ends_with("header/bpm") {
                        60.0 + v * 140.0
                    } else {
                        v
                    };
                    if self.pickup_allows(&m.address, v) {
                        self.route(&m.address, &[OscType::Float(scaled)]);
                    }
                }
            }
        }
    }

    /// A new default loop length (Settings): every deck's loop size and loop button use it.
    pub fn set_default_loop(&mut self, beats: f64) {
        let beats = beats.clamp(1.0 / 32.0, 64.0);
        for d in 0..NUM_DECKS {
            self.loop_size[d] = beats;
            self.deck_cmd(d, DeckCmd::DefaultLoopBeats(beats));
        }
        self.mark_dirty();
    }

    /// A hot cue pad pressed: an empty pad sets the cue; a set pad loops from it at the deck's loop
    /// size or jumps to it (Settings → Hot cue pads), the same on screen and on a controller.
    fn pad_press(&mut self, d: usize, idx: u8) -> Option<DeckCmd> {
        let set = self.snap.decks[d].hot_cues.get(idx as usize).is_some_and(|c| c.is_some());
        if set {
            self.decks[d].cue_chosen = true;
        }
        // Another deck is playing: a cue comes in in time with it (tempo and beat).
        let v = self.snap.decks[d];
        let other_playing = (0..self.deck_count()).any(|o| o != d && self.snap.decks[o].playing);
        if set && other_playing && v.has_grid && !v.sync && self.snap.clock.owner != ClockOwner::Deck(d as u8) {
            self.deck_cmd(d, DeckCmd::Sync(true));
        }
        if set && self.settings.cue_pads == crate::settings::CuePadMode::Loop {
            return Some(DeckCmd::CueBeatLoop { idx, beats: self.loop_size[d] });
        }
        self.deck_cmd(d, DeckCmd::HotCue { idx, pressed: true });
        Some(DeckCmd::HotCue { idx, pressed: false })
    }

    /// Set a control from dj itself (auto-mix, the fader popup's group buttons) and have MIDI pick
    /// it up: a controller's absolute fader or knob for it is ignored until moved to (or across)
    /// the new value, so it doesn't jump.
    pub fn set_picked_up(&mut self, addr: &str, v: f32) {
        self.route(addr, &[OscType::Float(v)]);
        if let Some((key, _)) = self.pickup_point(addr) {
            self.pickup.entry(key).or_insert(None);
        }
    }

    /// Whether a controller's absolute value `v` (0..1) for `addr` applies, under pick-up.
    pub(super) fn pickup_allows(&mut self, addr: &str, v: f32) -> bool {
        let Some((key, cur)) = self.pickup_point(addr) else { return true };
        let Some(last) = self.pickup.get(&key).copied() else { return true };
        let crossed = last.is_some_and(|l| (l - cur) * (v - cur) <= 0.0);
        if crossed || (v - cur).abs() < 0.03 {
            self.pickup.remove(&key);
            true
        } else {
            self.pickup.insert(key, Some(v));
            false
        }
    }

    /// Pick-up identity and current value (0..1) of a control. Addresses for the same control
    /// share it (`Deck/N/Tempo` 0..1 and `Deck/N/TempoSlider` -1..1).
    fn pickup_point(&self, addr: &str) -> Option<(String, f32)> {
        let a = addr.trim_matches('/').to_ascii_lowercase();
        let parts: Vec<&str> = a.split('/').collect();
        if let ["deck", n, "tempo" | "temposlider"] = parts.as_slice() {
            let d = n.parse::<usize>().ok()?.checked_sub(1)?;
            let v = self.snap.decks.get(d)?;
            return Some((format!("deck/{n}/tempo"), (v.tempo_fader as f32 + 1.0) / 2.0));
        }
        let cur = self.value_state(&a)?;
        Some((a, cur))
    }

    /// Current 0..1 value of a knob address (for relative encoders).
    pub fn value_state(&self, addr: &str) -> Option<f32> {
        let parts: Vec<String> = addr.trim_matches('/').split('/').map(|s| s.to_ascii_lowercase()).collect();
        let p: Vec<&str> = parts.iter().map(|s| s.as_str()).collect();
        let idx = |s: &str| s.trim_start_matches("ch").parse::<usize>().ok().and_then(|n| n.checked_sub(1)).filter(|i| *i < NUM_DECKS);
        Some(match p.as_slice() {
            ["mixer", c, "volume"] => self.session.channels[idx(c)?].params.volume,
            ["mixer", c, "trim"] => self.session.channels[idx(c)?].params.trim_db / 12.0 + 0.5,
            ["mixer", c, "eq", "low"] => self.session.channels[idx(c)?].params.eq[0],
            ["mixer", c, "eq", "mid"] => self.session.channels[idx(c)?].params.eq[1],
            ["mixer", c, "eq", "high"] => self.session.channels[idx(c)?].params.eq[2],
            ["mixer", c, "filter1"] => self.session.channels[idx(c)?].params.cutoff,
            ["mixer", c, "filter2"] => self.session.channels[idx(c)?].params.res,
            ["mixer", c, "filter3"] => self.session.channels[idx(c)?].params.width,
            ["mixer", c, "fx", "macro", n] => self.session.channels[idx(c)?].fx.macros[n.parse::<usize>().ok()?.checked_sub(1)?].value,
            ["deck", n, s] if stem_level(s).is_some() => self.snap.decks.get(n.parse::<usize>().ok()?.checked_sub(1)?)?.stem_gains[stem_level(s)?],
            ["header", "mastervolume"] => self.session.master.volume,
            ["header", "cuevolume"] => self.session.master.cue_volume,
            ["header", "cuemix"] => self.session.master.cue_mix,
            _ => return None,
        })
    }

    pub fn handle_key(&mut self, key: &str, pressed: bool) -> bool {
        let Some(addr) = self.profile.keys.iter().find(|(k, _)| k == key).map(|(_, a)| a.clone()) else { return false };
        match addr_kind(&addr) {
            Kind::Trigger | Kind::Step => {
                if pressed {
                    self.route(&addr, &[OscType::Int(1)]);
                }
            }
            Kind::Switch => {
                if pressed {
                    let cur = self.switch_state(&addr).unwrap_or(false);
                    self.route(&addr, &[OscType::Int(!cur as i32)]);
                }
            }
            _ => {
                self.route(&addr, &[OscType::Int(pressed as i32)]);
            }
        }
        true
    }

    // ------------------------------------------------------------ OSC

    pub fn start_osc(&mut self) {
        self.osc_in = None;
        self.osc_out = None;
        if !self.settings.osc_enabled {
            return;
        }
        match crate::osc::listen(self.settings.osc_port) {
            Ok(rx) => {
                self.osc_in = Some(rx);
            }
            Err(e) => self.log(format!("OSC: cannot listen on port {}: {e}", self.settings.osc_port)),
        }
        if self.settings.osc_feedback {
            match crate::osc::Sender::new(&self.settings.osc_feedback_host, self.settings.osc_feedback_port) {
                Ok(s) => self.osc_out = Some(s),
                Err(e) => self.log(format!("OSC feedback: {e}")),
            }
        }
        self.feedback.clear();
    }

    fn feedback_root(&self) -> String {
        let r = self.settings.osc_root.trim_end_matches('/');
        format!("{r}/")
    }

    fn send_feedback(&mut self, rel: &str, args: Vec<OscType>, force: bool) {
        let Some(out) = &self.osc_out else { return };
        if !force && self.feedback.get(rel) == Some(&args) {
            return;
        }
        let addr = format!("{}{}", self.feedback_root(), rel);
        out.send(&addr, args.clone());
        self.feedback.insert(rel.to_string(), args);
    }

    fn feedback_state(&mut self) {
        if self.osc_out.is_none() || self.last_feedback.elapsed() < Duration::from_millis(33) {
            return;
        }
        self.last_feedback = Instant::now();
        let time_tick = self.last_time_feedback.elapsed() >= Duration::from_secs(1);
        if time_tick {
            self.last_time_feedback = Instant::now();
        }
        let s = self.snap;
        let mut out: Vec<(String, Vec<OscType>)> = Vec::new();
        out.push(("Header/BPM".into(), vec![OscType::Float((s.clock.bpm * 100.0).round() as f32 / 100.0)]));
        out.push(("Header/MasterVolume".into(), vec![OscType::Float(self.session.master.volume)]));
        out.push(("Header/CueVolume".into(), vec![OscType::Float(self.session.master.cue_volume)]));
        for d in 0..self.deck_count() {
            let n = d + 1;
            let v = s.decks[d];
            let p = |x: &str| format!("Deck/{n}/{x}");
            if s.deck_mode[d] == DeckMode::Sampler {
                out.push((p("PlayerType"), vec![OscType::String("sampler".into())]));
                let sv = s.samplers[d];
                let lane = self.session.decks[d].sampler.selected_lane;
                out.push((p("Sampler/CurrentStep"), vec![OscType::Int(if sv.playing { sv.current_step[lane] as i32 + 1 } else { 0 })]));
                out.push((p("Sampler/Lane"), vec![OscType::Int(lane as i32 + 1)]));
                continue;
            }
            out.push((p("PlayerType"), vec![OscType::String(if v.loaded { "track" } else { "empty" }.into())]));
            out.push((p("Loaded"), vec![OscType::Int(v.loaded as i32)]));
            if !v.loaded {
                continue;
            }
            out.push((p("Playing"), vec![OscType::Int(v.playing as i32)]));
            out.push((p("PlayPending"), vec![OscType::Int(v.play_pending as i32)]));
            if let Some(info) = &self.decks[d].info {
                let key = self.key_text(info.tags.key);
                let bars = (v.duration_secs * v.track_bpm / 60.0 / 4.0) as i32;
                out.push((
                    p("TrackInfo"),
                    vec![
                        OscType::String(info.title()),
                        OscType::String(info.tags.artist.clone().unwrap_or_default()),
                        OscType::String(key),
                        OscType::Float(v.track_bpm as f32),
                        OscType::Float(v.duration_secs as f32),
                        OscType::Int(bars),
                        OscType::Int(bars * 4),
                        OscType::String("track".into()),
                    ],
                ));
            }
            out.push((p("Position"), vec![OscType::Float(((v.position_secs * 10.0).round() / 10.0) as f32)]));
            let prog = if v.duration_secs > 0.0 { v.position_secs / v.duration_secs } else { 0.0 };
            out.push((p("Progress"), vec![OscType::Float(((prog * 500.0).round() / 500.0) as f32)]));
            if time_tick {
                out.push((p("Time"), vec![OscType::Float(v.position_secs.floor() as f32)]));
            }
            out.push((p("BPM"), vec![OscType::Float(((v.bpm * 100.0).round() / 100.0) as f32)]));
            out.push((p("Sync"), vec![OscType::Int(v.sync as i32)]));
            out.push((p("Loop"), vec![OscType::Int(v.loop_on as i32)]));
            out.push((
                p("LoopInfo"),
                vec![OscType::Int(v.loop_on as i32), OscType::Float(v.loop_start_secs as f32), OscType::Float(v.loop_end_secs as f32)],
            ));
            out.push((p("Pitch"), vec![OscType::Float(v.semitones as f32)]));
            out.push((p("TempoSlider"), vec![OscType::Float(((v.tempo_fader * 1000.0).round() / 1000.0) as f32)]));
            let follower = v.sync && s.clock.owner != ClockOwner::Deck(d as u8);
            out.push((p("TempoSliderEnabled"), vec![OscType::Int(!follower as i32)]));
            out.push((p("PhaseOffset"), vec![OscType::Float(((v.phase_offset * 1000.0).round() / 1000.0) as f32)]));
            let mut cues = Vec::new();
            for (i, c) in v.hot_cues.iter().enumerate() {
                cues.push(OscType::Int(if c.is_some() { i as i32 + 1 } else { 0 }));
                let name = self.decks[d].info.as_ref().and_then(|x| x.cues[i].as_ref()).map(|c| c.name.clone()).unwrap_or_default();
                cues.push(OscType::String(name));
            }
            out.push((p("Cues"), cues));
            if v.playing && v.has_grid {
                out.push((p("BarRamp"), vec![OscType::Float((((v.beat / 4.0).rem_euclid(1.0)) * 50.0).round() as f32 / 50.0)]));
            }
        }
        for c in 0..self.deck_count() {
            let n = c + 1;
            let ch = &self.session.channels[c];
            let p = |x: &str| format!("Mixer/CH{n}/{x}");
            let cp = ch.params;
            out.push((p("Volume"), vec![OscType::Float(cp.volume)]));
            out.push((p("Trim"), vec![OscType::Float(cp.trim_db / 12.0 + 0.5)]));
            out.push((p("EQ/Low"), vec![OscType::Float(cp.eq[0])]));
            out.push((p("EQ/Mid"), vec![OscType::Float(cp.eq[1])]));
            out.push((p("EQ/High"), vec![OscType::Float(cp.eq[2])]));
            out.push((p("Filter1"), vec![OscType::Float(cp.cutoff)]));
            out.push((p("Filter2"), vec![OscType::Float(cp.res)]));
            out.push((p("Filter3"), vec![OscType::Float(cp.width)]));
            out.push((p("Mute"), vec![OscType::Int(cp.mute as i32)]));
            out.push((p("Solo"), vec![OscType::Int(cp.solo as i32)]));
            out.push((p("Cue"), vec![OscType::Int(cp.cue as i32)]));
            out.push((p("FX"), vec![OscType::Int(!ch.fx.bypass_all as i32)]));
            for (i, m) in ch.fx.macros.iter().enumerate() {
                out.push((format!("Mixer/CH{n}/FX/Macro/{}", i + 1), vec![OscType::Float(m.value)]));
            }
        }
        for (rel, args) in out {
            self.send_feedback(&rel, args, false);
        }
    }

    fn midi_feedback(&mut self) {
        if !self.midi.has_output() {
            return;
        }
        let maps: Vec<MidiMapping> = self.profile.mappings.iter().filter(|m| m.feedback).cloned().collect();
        for m in maps {
            let Some(state) = self.switch_state(&m.address) else { continue };
            let v = if state { 127 } else { 0 };
            if self.midi_led.get(&m.source) == Some(&v) {
                continue;
            }
            self.midi_led.insert(m.source, v);
            let msg = match m.source.kind {
                MidiKind::Note => [0x90 | m.source.channel, m.source.number, v],
                MidiKind::Cc => [0xb0 | m.source.channel, m.source.number, v],
                MidiKind::PitchBend => continue,
            };
            self.midi.send(&msg);
        }
    }

    // ------------------------------------------------------------ main loop

    /// Poll everything; call ~60×/s from the UI or headless loop.
    pub fn tick(&mut self) {
        self.awake.set(self.settings.keep_awake);
        self.snap = *self.ctl.snapshot();
        self.midi.set_clock(self.snap.clock.bpm);

        while let Some(ev) = self.ctl.poll_event() {
            match ev {
                Event::Garbage(g) => drop_garbage(g),
                Event::Deck { deck, event } => {
                    let d = deck as usize;
                    match event {
                        DeckEvent::CueSet { idx, secs, end_secs } => {
                            let cue = Cue {
                                kind: if end_secs.is_some() { CueKind::Loop } else { CueKind::Hot },
                                secs,
                                end_secs,
                                name: String::new(),
                                color: CUE_COLORS[idx as usize % CUE_COLORS.len()],
                            };
                            self.persist_cue(d, idx, Some(cue));
                            self.send_feedback(&format!("Deck/{}/CuePress", d + 1), vec![OscType::Int(idx as i32 + 1), OscType::Int(1)], true);
                        }
                        DeckEvent::CueDeleted { idx } => self.persist_cue(d, idx, None),
                        DeckEvent::CueFire { idx } => {
                            self.send_feedback(&format!("Deck/{}/CueFire", d + 1), vec![OscType::Int(idx as i32 + 1)], true);
                        }
                        DeckEvent::PlayState { .. } | DeckEvent::EndOfTrack => {}
                    }
                }
                Event::Beat { bar, beat } => {
                    self.beat_flash = (bar, beat, Instant::now());
                    self.midi.align_beat();
                    self.send_feedback("Header/Clock", vec![OscType::Int(beat as i32)], true);
                    for d in 0..self.deck_count() {
                        let v = self.snap.decks[d];
                        if v.playing && v.has_grid {
                            let b = v.beat.floor() as i64;
                            if b != self.decks[d].last_beat {
                                self.decks[d].last_beat = b;
                                let bar = b.div_euclid(4) + 1;
                                let bt = b.rem_euclid(4) + 1;
                                self.send_feedback(
                                    &format!("Deck/{}/Transport", d + 1),
                                    vec![OscType::Int(bar as i32), OscType::Int(bt as i32)],
                                    true,
                                );
                            }
                        }
                    }
                }
                Event::FxFailed { ch, slot } => self.log(format!("CH{} insert {} failed and was bypassed", ch + 1, slot + 1)),
                Event::ClockOwner(_) => {}
                Event::RecordingOverflow => self.log("Recording buffer overflow (disk too slow?)"),
            }
        }

        while let Some(u) = self.stems.try_recv() {
            self.on_stems_update(u);
        }

        while let Ok(r) = self.loader_rx.try_recv() {
            match r {
                LoadResult::Track { deck, path, result } => {
                    if self.decks[deck].loading.as_ref() != Some(&path) {
                        continue; // superseded
                    }
                    match result {
                        Ok(lt) => self.on_track_loaded(deck, lt),
                        Err(e) => {
                            self.decks[deck].loading = None;
                            self.decks[deck].error = Some(e.clone());
                            self.log(format!("Deck {}: {e}", deck + 1));
                            if self.autodj.as_mut().is_some_and(|a| a.skip(&path)) {
                                self.log(format!("Auto-DJ: skipped {} (it didn't load)", path.display()));
                            }
                        }
                    }
                }
                LoadResult::Sample { deck, lane, path, result } => match result {
                    Ok(a) => self.send(Command::SamplerSample { deck: deck as u8, lane: lane as u8, audio: Some(a) }),
                    Err(e) => self.log(format!("Sample {}: {e}", path.display())),
                },
            }
        }

        if let Some(rx) = &self.library_rx {
            if let Ok(mut v) = rx.try_recv() {
                self.fill_detected(&mut v);
                self.library = v;
                self.library_scanning = false;
                self.library_rx = None;
                let _ = crate::settings::save_json_later(&crate::settings::config_dir().join("library-cache.json"), &self.library);
                if self.library.is_empty() && !self.settings.library_folders.is_empty() {
                    self.log("No music files found (MP3, WAV, AIFF, FLAC, Ogg, M4A) in the library folders");
                } else {
                    self.log(format!("Library: {} tracks", self.library.len()));
                }
            }
        }
        if let Some(rx) = &self.vst3_rx {
            if let Ok(c) = rx.try_recv() {
                self.vst3_scanning = false;
                self.vst3_rx = None;
                self.apply_scan(c);
                self.log(format!("VST3: {} effects, {} failed bundles", self.catalog_vst3.len(), self.vst3_errors.len()));
            }
        }

        // OSC input.
        let mut msgs = Vec::new();
        if let Some(rx) = &self.osc_in {
            while let Ok(m) = rx.try_recv() {
                msgs.push(m);
            }
        }
        for (addr, args) in msgs {
            if let Some(rel) = self.strip_root(&addr).map(|s| s.to_string()) {
                if !self.route(&rel, &args) {
                    self.log(format!("OSC: unknown address {addr}"));
                }
            }
        }

        // MIDI input.
        for m in self.midi.poll() {
            self.handle_midi(&m);
        }
        self.jog_tick();

        // VST3 latency changes and in-plugin edits.
        for ch in 0..NUM_DECKS {
            for slot in 0..MAX_SLOTS {
                if let Some(FxLive::Vst3(h)) = self.fx_live[ch][slot].clone() {
                    let flags = h.take_restart_flags();
                    if dj_vst3::Vst3Handle::latency_changed(flags) {
                        self.send(Command::FxLatency { ch: ch as u8, slot: slot as u8, samples: h.latency() });
                    }
                    for (id, v) in h.take_edits() {
                        if let Some(i) = h.param_index(id) {
                            self.set_fx_param(ch, slot, i, v as f32);
                        }
                    }
                }
            }
        }

        self.automix_tick();
        // (After the mix's tick: a mix Auto-DJ starts is seen by the engine before it's checked.)
        self.autodj_tick();
        self.prepare_tick();
        self.feedback_state();
        self.midi_feedback();

        if let Some(t) = self.dirty_since {
            if t.elapsed() > Duration::from_secs(3) {
                self.save_all();
            }
        }
    }

    /// Before quitting: everything saved, and a recording finished (its file complete).
    pub fn prepare_to_quit(&mut self) {
        if let Some(mut r) = self.recorder.take() {
            self.send(Command::StopRecording);
            if let Some(t) = r.thread.take() {
                // (The writer finishes the WAV once the engine stops feeding it; not forever.)
                let (tx, rx) = crossbeam_channel::bounded(1);
                std::thread::spawn(move || {
                    let _ = t.join();
                    let _ = tx.send(());
                });
                let _ = rx.recv_timeout(std::time::Duration::from_secs(3));
            }
        }
        self.stop_preparing();
        // A mix's echo-out not removed yet (its tail still ringing, or the mix still running):
        // gone before the session is saved, else it would come back on that channel next time.
        self.cancel_mix();
        self.drop_mix_echoes();
        self.save_all();
        crate::settings::flush_writes();
    }

    pub fn save_all(&mut self) {
        self.dirty_since = None;
        self.snapshot_plugin_states();
        // (Written in the background: a big track database never holds up the screen.)
        let _ = self.session.save_later();
        let _ = self.settings.save_later();
        let _ = self.trackdb.save_later();
    }

    pub fn macro_label(&self, ch: usize, idx: usize) -> String {
        let m: &MacroModel = &self.session.channels[ch].fx.macros[idx];
        match m.targets.as_slice() {
            [] => "—".into(),
            [t] => {
                let p = self.fx_live[ch][t.slot as usize].as_ref().map(|l| l.param_name(t.param as usize)).unwrap_or_default();
                format!("{}·{}", t.slot + 1, p)
            }
            ts => format!("{} params", ts.len()),
        }
    }
}

/// A jog wheel: how long a nudge outlasts the last tick, and ticks per beat when moving through a
/// paused track (Pioneer jogs send about 720 a turn: some 4 beats, like a record).
const JOG_HOLD_MS: u64 = 80;
const JOG_TICKS_PER_BEAT: f64 = 180.0;

/// Default cue palette (cues set in Perform).
pub const CUE_COLORS: [[u8; 3]; 8] = [
    [0xe6, 0x3b, 0x3b],
    [0xf2, 0x8c, 0x28],
    [0xf2, 0xd1, 0x28],
    [0x5b, 0xc2, 0x36],
    [0x2d, 0xc8, 0xc8],
    [0x3b, 0x7b, 0xe6],
    [0x9b, 0x59, 0xe6],
    [0xe6, 0x4b, 0xb4],
];

fn drop_garbage(g: Garbage) {
    // Dropping here (UI thread) is the whole point: plugin teardown and
    // large deallocations stay off the audio thread.
    drop(g);
}

/// Merge detected cue points into a track's pads: a detected cue within `close` seconds of a cue
/// already there replaces it (on the same pad); the others go on free pads, earliest first; cues
/// with nothing detected near them stay. Returns the pads and how many were replaced, added and
/// left out (no free pad).
pub fn merge_cues(old: &[Option<Cue>; NUM_HOT_CUES], mut found: Vec<Cue>, close: f64) -> ([Option<Cue>; NUM_HOT_CUES], usize, usize, usize) {
    found.sort_by(|a, b| a.secs.total_cmp(&b.secs));
    let mut pads = old.clone();
    let mut touched = [false; NUM_HOT_CUES];
    let (mut replaced, mut added, mut dropped) = (0, 0, 0);
    let near = |pads: &[Option<Cue>; NUM_HOT_CUES], i: usize, t: f64| pads[i].as_ref().map(|c| (c.secs - t).abs()).filter(|x| *x <= close);
    for c in found {
        // The nearest cue already there that nothing replaced yet.
        let nearest = (0..NUM_HOT_CUES).filter(|&i| !touched[i]).filter_map(|i| near(&pads, i, c.secs).map(|x| (i, x))).min_by(|a, b| a.1.total_cmp(&b.1));
        if let Some((i, _)) = nearest {
            pads[i] = Some(c);
            touched[i] = true;
            replaced += 1;
        } else if (0..NUM_HOT_CUES).any(|i| touched[i] && near(&pads, i, c.secs).is_some()) {
            // Next to one this detection just set: the same section.
        } else if let Some(i) = (0..NUM_HOT_CUES).find(|&i| pads[i].is_none()) {
            pads[i] = Some(c);
            touched[i] = true;
            added += 1;
        } else {
            dropped += 1;
        }
    }
    (pads, replaced, added, dropped)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use dj_engine::{create, EngineConfig};

    /// Tests share the process-wide `DJ_CONFIG_DIR`; hold this for the whole test.
    static CONFIG_DIR_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// A test's config folder (settings, prepared stems, generated audio): removed when the test
    /// ends, so runs don't pile up gigabytes in the temp folder.
    pub(crate) struct TestDir {
        dir: PathBuf,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    pub(crate) fn core() -> (dj_engine::Engine, Core, TestDir) {
        let lock = CONFIG_DIR_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = std::env::temp_dir().join(format!("dj-core-test-{}-{:?}", std::process::id(), std::thread::current().id()));
        let _ = std::fs::remove_dir_all(&dir);
        // Folders of earlier runs (a background stems write can land after a test ends).
        let mine = format!("dj-core-test-{}-", std::process::id());
        for e in std::fs::read_dir(std::env::temp_dir()).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with("dj-core-test-") && !name.starts_with(&mine) {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
        std::env::set_var("DJ_CONFIG_DIR", &dir);
        let guard = TestDir { dir, _lock: lock };
        let (engine, ctl) = create(EngineConfig { sample_rate: 48000.0, max_block: 512, max_latency: 4096 });
        // Stems from the tiny stand-in HT-Demucs model (stems-core's test data).
        let demucs = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../stems-core/tests/data/demucs_test.onnx"));
        let settings = Settings { osc_enabled: false, vst3_enabled: false, library_folders: vec![], stems_demucs: Some(demucs), song_sections: false, ..Default::default() };
        let mut c = Core::new(ctl, 48000.0, 512, settings, Session::default());
        c.sync_engine();
        (engine, c, guard)
    }

    fn pump(e: &mut dj_engine::Engine, c: &mut Core) {
        for _ in 0..4 {
            e.process(512);
        }
        c.tick();
    }

    #[test]
    fn fx_chain_macros_and_presets() {
        let (mut e, mut c, _config) = core();
        assert!(c.route("Mixer/CH1/FX/Insert", &[OscType::String("native:autofilter".into())]));
        c.route("Mixer/CH1/FX/Insert", &[OscType::String("native:echo".into()), OscType::String("post".into())]);
        pump(&mut e, &mut c);
        assert_eq!(c.snap.channels[0].fx.iter().flatten().count(), 2);
        assert_eq!(c.session.channels[0].fx.slots[1].as_ref().unwrap().pos, InsertPos::Post);

        // A macro driving two parameters with different ranges.
        c.assign_macro(0, 0, 0, 0);
        c.assign_macro(0, 0, 1, 1);
        c.set_macro_target(0, 0, 1, |t| {
            t.min = 0.2;
            t.max = 0.4;
        });
        assert!(c.route("Mixer/CH1/FX/Macro/1", &[OscType::Float(0.5)]));
        let s0 = c.session.channels[0].fx.slots[0].as_ref().unwrap().params[0];
        let s1 = c.session.channels[0].fx.slots[1].as_ref().unwrap().params[1];
        assert!((s0 - 0.5).abs() < 1e-6 && (s1 - 0.3).abs() < 1e-6, "{s0} {s1}");

        // FX switch = bypass all inverted.
        c.route("Mixer/CH1/FX", &[OscType::Int(0)]);
        assert!(c.session.channels[0].fx.bypass_all);

        // Reorder keeps macro targets attached to their effects.
        c.move_insert(0, 0, 1);
        assert_eq!(c.session.channels[0].fx.slots[1].as_ref().unwrap().name, "Auto Filter");
        assert!(c.session.channels[0].fx.macros[0].targets.iter().any(|t| t.slot == 1 && t.param == 0));

        // Preset round trip into another channel.
        c.save_fx_preset(0, "test chain");
        let (_, path) = FxChainModel::list_presets().into_iter().find(|(n, _)| n == "test chain").unwrap();
        c.load_fx_preset(2, &path);
        pump(&mut e, &mut c);
        let a = &c.session.channels[0].fx;
        let b = &c.session.channels[2].fx;
        assert_eq!(a.slots.iter().flatten().map(|s| &s.name).collect::<Vec<_>>(), b.slots.iter().flatten().map(|s| &s.name).collect::<Vec<_>>());
        assert_eq!(a.macros, b.macros);
        assert!(b.bypass_all);
        assert_eq!(c.snap.channels[2].fx.iter().flatten().count(), 2);

        c.remove_insert(0, 1);
        assert!(c.session.channels[0].fx.macros[0].targets.iter().all(|t| t.slot != 1));
    }

    #[test]
    fn stem_switches_route_to_the_engine_and_report_state() {
        let (mut e, mut c, _config) = core();
        assert_eq!(addr_kind("Deck/1/StemDrums"), Kind::Switch);
        assert_eq!(addr_kind("Deck/1/Acapella"), Kind::Switch);
        assert!(c.route("Deck/2/StemVocals", &[OscType::Int(0)]));
        pump(&mut e, &mut c);
        assert_eq!(c.snap.decks[1].stem_gains, STEMS_INSTRUMENTAL);
        assert_eq!(c.switch_state("/deck/2/stemvocals"), Some(false));
        assert_eq!(c.switch_state("/deck/2/stembass"), Some(true));
        assert_eq!(c.switch_state("/deck/2/instrumental"), Some(true));

        assert!(c.route("Deck/2/Acapella", &[OscType::Int(1)]));
        pump(&mut e, &mut c);
        assert_eq!(c.snap.decks[1].stem_gains, STEMS_ACAPELLA);
        assert_eq!(c.switch_state("/deck/2/acapella"), Some(true));
        assert_eq!(c.switch_state("/deck/2/instrumental"), Some(false));

        c.route("Deck/2/Acapella", &[OscType::Int(0)]);
        pump(&mut e, &mut c);
        assert_eq!(c.snap.decks[1].stem_gains, [1.0; 4]);

        // Continuous levels, for faders.
        assert_eq!(addr_kind("Deck/1/StemVocalsLevel"), Kind::Value);
        assert!(c.route("Deck/2/StemVocalsLevel", &[OscType::Float(0.25)]));
        pump(&mut e, &mut c);
        assert_eq!(c.snap.decks[1].stem_gains, [1.0, 1.0, 0.25, 1.0]);
    }

    /// A kick on every beat at 120 BPM.
    fn kick_wav(name: &str, secs: usize) -> PathBuf {
        let dir = crate::settings::config_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        let spec = hound::WavSpec { channels: 2, sample_rate: 44100, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
        let mut w = hound::WavWriter::create(&path, spec).unwrap();
        for i in 0..44100 * secs {
            let t = (i % 22050) as f32 / 44100.0;
            let v = 0.6 * (std::f32::consts::TAU * 55.0 * t).sin() * (-t * 18.0).exp();
            let s = (v * 30000.0) as i16;
            w.write_sample(s).unwrap();
            w.write_sample(s).unwrap();
        }
        w.finalize().unwrap();
        path
    }

    #[test]
    fn a_track_that_was_on_a_deck_can_be_scored_for_the_next_mix() {
        let (mut e, mut c, _config) = core();
        let next = kick_wav("next-up.wav", 60);
        // On a deck once: its profile is kept (stems in, here without sections).
        c.load_track(1, &next);
        let deadline = Instant::now() + std::time::Duration::from_secs(60);
        while c.decks[1].stems_source.is_none() {
            assert!(Instant::now() < deadline, "stems never came");
            pump(&mut e, &mut c);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        pump(&mut e, &mut c);
        c.eject(1);
        c.profiles.clear();
        assert!(c.profile(&next).is_some_and(|p| p.stems), "profile kept on disk");
        // Another track plays: how would `next` mix in after it?
        c.load_track(0, &kick_wav("playing-now.wav", 90));
        let deadline = Instant::now() + std::time::Duration::from_secs(60);
        while !(c.snap.decks[0].loaded && c.decks[0].info.is_some() && c.decks[0].stem_wave.is_some()) {
            assert!(Instant::now() < deadline, "never loaded");
            pump(&mut e, &mut c);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        c.deck_cmd(0, DeckCmd::Play);
        pump(&mut e, &mut c);
        let s = c.mix_score(0, &next).expect("a profile, so a score");
        assert!(s.score > 0.0 && s.why.contains("bars"), "{s:?}");
        assert_eq!(c.mix_score_known(0, &next), Some(Some(s)), "cached");
        assert_eq!(c.mix_score(0, &next.with_file_name("never-prepared.wav")), None);
    }

    #[test]
    fn auto_dj_plays_the_list_as_a_set() {
        let (mut e, mut c, _config) = core();
        let tracks: Vec<PathBuf> = (0..3).map(|k| kick_wav(&format!("set-{k}.wav"), 40)).collect();
        c.set_autodj(Some(tracks.clone()));
        let deadline = Instant::now() + std::time::Duration::from_secs(600);
        let mut mixes = 0;
        let mut was_mixing = false;
        // What happened, for a failure: each change of the decks' tracks and Auto-DJ's played list.
        let mut history: Vec<String> = Vec::new();
        let name = |p: &Path| p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        // Until the last track has played out.
        while c.autodj.is_some() {
            assert!(Instant::now() < deadline, "the set never ended: {:?}", c.status);
            pump(&mut e, &mut c);
            let decks: Vec<String> = (0..2)
                .map(|d| {
                    let v = c.snap.decks[d];
                    format!(
                        "{}{}{}",
                        c.decks[d].info.as_ref().map(|i| name(&i.path)).unwrap_or("-".into()),
                        if v.playing { "▶" } else { "" },
                        c.decks[d].loading.as_ref().map(|p| format!("(loading {})", name(p))).unwrap_or_default()
                    )
                })
                .collect();
            let mut played: Vec<String> = c.autodj.as_ref().map(|a| a.played.iter().map(|p| name(p)).collect()).unwrap_or_default();
            played.sort();
            let state = format!("{decks:?} played {played:?} mix {}", c.automix.is_some());
            if history.last().is_none_or(|h| !h.ends_with(&state)) {
                history.push(format!("@{:.1} {state}", c.snap.clock.beat));
            }
            // Once the set is going, the next track loads while one still plays (not after it ended).
            if c.autodj.as_ref().is_some_and(|a| !a.played.is_empty()) && (0..2).any(|d| c.decks[d].loading.is_some()) {
                assert!((0..2).any(|d| c.snap.decks[d].playing), "loaded after the music stopped\n{}", history.join("\n"));
            }
            if was_mixing && c.automix.is_none() {
                mixes += 1;
            }
            was_mixing = c.automix.is_some();
            // Never two tracks heard at full at once outside a mix.
            if c.automix.is_none() {
                let loud = (0..2).filter(|&d| c.snap.decks[d].playing && !c.snap.decks[d].loop_on && c.session.channels[d].params.volume > 0.5).count();
                assert!(loud <= 1, "two tracks up outside a mix");
            }
        }
        assert_eq!(mixes, 2, "three tracks, two mixes: {:?}\n{}", c.status, history.join("\n"));
        // The next track loading on the old deck before a mix was over (its old track done)
        // ended that mix as done, not cancelled.
        assert!(!c.status.iter().any(|(_, m)| m.contains("cancelled")), "{:?}", c.status);
        assert!(c.status.iter().any(|(_, m)| m.contains("last track")), "{:?}", c.status);
        assert!(c.status.iter().any(|(_, m)| m.contains("the set is over")), "{:?}", c.status);
        for t in &tracks {
            assert!(c.history.contains(t), "{} played", t.display());
        }
        // Each track loaded once (not over itself while the engine caught up): count each load
        // where it starts (one can span several lines, a mix ending meanwhile).
        let loading = |h: &str| h.split("(loading ").skip(1).map(|r| r.split(')').next().unwrap_or("").to_string()).collect::<Vec<_>>();
        let loads = history.iter().enumerate().map(|(k, h)| loading(h).into_iter().filter(|l| k == 0 || !loading(&history[k - 1]).contains(l)).count()).sum::<usize>();
        assert!(loads <= 3, "{loads} loads\n{}", history.join("\n"));
    }

    #[test]
    fn auto_dj_with_a_big_crate_stays_quick_and_skips_tracks_that_dont_load() {
        let (mut e, mut c, _config) = core();
        let playing = kick_wav("big-crate-playing.wav", 40);
        // A big library, all of it in the crate; the best next pick (first, a tempo like the
        // playing track's) doesn't load.
        let dir = playing.parent().unwrap().to_path_buf();
        let broken = dir.join("big-crate-broken.wav");
        std::fs::write(&broken, b"not audio").unwrap();
        let mut library: Vec<dj_library::scan::Entry> =
            (0..20_000).map(|k| dj_library::scan::Entry { path: dir.join(format!("folder/sub/track-{k:05}.wav")), ..Default::default() }).collect();
        library.insert(0, dj_library::scan::Entry { path: broken.clone(), bpm: Some(120.0), ..Default::default() });
        library.push(dj_library::scan::Entry { path: playing.clone(), bpm: Some(120.0), ..Default::default() });
        c.library = library;
        c.load_track(0, &playing);
        let deadline = Instant::now() + std::time::Duration::from_secs(60);
        while !(c.snap.decks[0].loaded && c.decks[0].info.is_some()) {
            assert!(Instant::now() < deadline, "never loaded");
            pump(&mut e, &mut c);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        c.deck_cmd(0, DeckCmd::Play);
        while !c.snap.decks[0].playing {
            assert!(Instant::now() < deadline, "never played");
            pump(&mut e, &mut c);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        c.set_autodj(Some(c.library.iter().map(|e| e.path.clone()).collect()));
        // Picking from 20 000 tracks: well under a frame's worth of a second, even unoptimised.
        let t = Instant::now();
        pump(&mut e, &mut c);
        assert!(t.elapsed().as_millis() < 500, "a pick took {} ms", t.elapsed().as_millis());
        assert_eq!(c.decks[1].loading.as_deref(), Some(broken.as_path()), "the best fit first");
        // It doesn't load: skipped, and the next pick (a second later) loads another track.
        let deadline = Instant::now() + std::time::Duration::from_secs(30);
        while !c.status.iter().any(|(_, m)| m.contains("Auto-DJ: skipped")) {
            assert!(Instant::now() < deadline, "never skipped: {:?}", c.status);
            pump(&mut e, &mut c);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(c.autodj.as_ref().unwrap().played.contains(&broken));
        let deadline = Instant::now() + std::time::Duration::from_secs(30);
        while c.decks[1].loading.as_deref().is_none_or(|p| p == broken) {
            assert!(Instant::now() < deadline, "never picked again: {:?}", c.status);
            pump(&mut e, &mut c);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        c.set_autodj(None);
    }

    #[test]
    fn tracks_are_prepared_in_the_background() {
        let (mut e, mut c, _config) = core();
        let path = kick_wav("prepare-me.wav", 30);
        c.prepare_tracks(vec![path.clone(), path.with_file_name("missing.wav")]);
        let deadline = Instant::now() + std::time::Duration::from_secs(600);
        while c.preparer.is_some() {
            assert!(Instant::now() < deadline, "never finished");
            pump(&mut e, &mut c);
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(c.status.iter().any(|(_, m)| m.contains("Prepared 1 of 2 tracks") && m.contains("1 failed")), "{:?}", c.status);
        let p = c.profile(&path).expect("a mix profile");
        assert!(p.stems && (p.bpm - 120.0).abs() < 0.5, "stems {} bpm {}", p.stems, p.bpm);
        assert!((14..=15).contains(&p.bars().len()), "30 s at 120 BPM: 15 bars (from its first beat): {}", p.bars().len());
        // How much of it has vocals: kept for the library.
        assert!(p.vocals().is_some());
        assert_eq!(c.trackdb.tracks.get(&TrackDb::key(&path)).and_then(|o| o.vocals), p.vocals());
        // Then it loads with its stems ready (prepared), and preparing again skips it.
        c.load_track(0, &path);
        let deadline = Instant::now() + std::time::Duration::from_secs(60);
        while c.decks[0].stems_source.is_none() {
            assert!(Instant::now() < deadline, "stems never came");
            pump(&mut e, &mut c);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(c.decks[0].stems_source.as_deref(), Some("prepared stems"));
        c.prepare_tracks(vec![path.clone()]);
        while c.preparer.is_some() {
            pump(&mut e, &mut c);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(c.status.iter().any(|(_, m)| m.contains("Prepared 0 of 1 tracks (1 already were)")), "{:?}", c.status);
    }

    #[test]
    fn mix_style_is_remembered_and_the_popup_says_what_each_does() {
        use crate::settings::MixStyle;
        let (mut e, mut c, _config) = core();
        mix_setup(&mut e, &mut c);
        assert_eq!(c.settings.mix_style, MixStyle::Auto);
        c.set_mix_style(MixStyle::Drop);
        assert_eq!(c.settings.mix_style, MixStyle::Drop);
        assert!(c.dirty_since.is_some(), "saved");
        let drop = c.plan_now(1, 0, c.settings.mix_style);
        assert_eq!(drop.how, automix::MixHow::Drop);
        let s = crate::gui::deck::plan_summary(&c, &drop);
        assert!(s.starts_with("in ") || s.starts_with("on the next bar"), "{s}");
        assert!(s.ends_with(" · Drop"), "a cut has no length: {s}");
        // Blend: stems when both decks have them, else EQ, and the blend's length.
        let blend = c.plan_now(1, 0, MixStyle::Stems);
        assert_ne!(blend.how, automix::MixHow::Drop);
        let s = crate::gui::deck::plan_summary(&c, &blend);
        assert!(s.contains(&format!("{:.0} bars", blend.length_beats / 4.0)), "{s}");
    }

    #[test]
    fn the_beats_all_in_one_hears_correct_the_grid() {
        use crate::stems::{Beat, Structure, Update};
        let (mut e, mut c, _config) = core();
        c.load_track(0, &kick_wav("grid-check.wav", 40));
        let deadline = Instant::now() + std::time::Duration::from_secs(60);
        while !(c.snap.decks[0].loaded && c.decks[0].info.is_some()) {
            assert!(Instant::now() < deadline, "track never loaded");
            pump(&mut e, &mut c);
        }
        let g = c.decks[0].track.as_ref().unwrap().grid.expect("a detected grid");
        assert!((g.bpm - 120.0).abs() < 0.5, "{g:?}");
        let id = c.decks[0].info.as_ref().unwrap().track_id;
        // The model hears every kick, the bars starting on the second (0.5 s), which the grid
        // doesn't have on a bar line.
        assert!(g.beat_at(0.5).round().rem_euclid(4.0) != 0.0, "{g:?}");
        let beats: Vec<Beat> = (0..80).map(|k| Beat { secs: 0.5 * k as f64, downbeat: if k % 4 == 1 { 0.9 } else { 0.1 } }).collect();
        let found = Arc::new(Structure { sections: vec![], beats });
        let beat_now = |e: &mut dj_engine::Engine, c: &mut Core| {
            pump(e, c);
            c.snap.decks[0].beat
        };
        c.on_stems_update(Update::Sections { deck: 0, track_id: id, found: found.clone(), cues: None, drums: None });
        assert!(c.status.iter().any(|(_, m)| m.contains("corrected") && m.contains("bars start")), "{:?}", c.status);
        // Now the heard downbeat at 0.5 s is a bar's first beat (the deck sits at 0 s).
        let at_half = beat_now(&mut e, &mut c) + 1.0;
        assert!((at_half - (at_half / 4.0).round() * 4.0).abs() < 0.05, "beat at 0.5 s: {at_half}");
        // Cue points on the grid's bar starts (Mixed In Key's phrases) keep its bars.
        c.deck_cmd(0, DeckCmd::SetGrid(None));
        for (i, t) in [8.0, 16.0, 32.0].into_iter().enumerate() {
            c.decks[0].info.as_mut().unwrap().cues[i] = Some(Cue { kind: CueKind::Hot, secs: g.secs_at_beat(t * 2.0), end_secs: None, name: String::new(), color: [0; 3] });
        }
        c.status.clear();
        c.on_stems_update(Update::Sections { deck: 0, track_id: id, found, cues: None, drums: None });
        assert!(!c.status.iter().any(|(_, m)| m.contains("corrected")), "{:?}", c.status);
        assert!((beat_now(&mut e, &mut c) - g.beat_at(0.0)).abs() < 0.05, "grid kept");
        assert_eq!(c.decks[0].sections, None, "no sections found: none shown");
    }

    #[test]
    fn steady_heard_beats_correct_a_detected_bpm_and_it_is_kept() {
        use crate::stems::{Beat, Structure, Update};
        let (mut e, mut c, _config) = core();
        let path = kick_wav("retempo.wav", 40);
        c.load_track(0, &path);
        let deadline = Instant::now() + std::time::Duration::from_secs(60);
        while !(c.snap.decks[0].loaded && c.decks[0].info.is_some()) {
            assert!(Instant::now() < deadline, "track never loaded");
            pump(&mut e, &mut c);
        }
        assert_eq!(c.decks[0].info.as_ref().unwrap().grid_source, Some(GridSource::Detected));
        let id = c.decks[0].info.as_ref().unwrap().track_id;
        // The model hears a steady beat every 0.75 s (80 BPM; the detector said 120), the bars
        // starting on the first.
        let beats: Vec<Beat> = (0..52).map(|k| Beat { secs: 0.75 * k as f64, downbeat: if k % 4 == 0 { 0.9 } else { 0.1 } }).collect();
        let found = Arc::new(Structure { sections: vec![], beats });
        c.on_stems_update(Update::Sections { deck: 0, track_id: id, found: found.clone(), cues: None, drums: None });
        assert!(c.status.iter().any(|(_, m)| m.contains("BPM 120.0 → 80.0")), "{:?}", c.status);
        pump(&mut e, &mut c);
        assert!((c.snap.decks[0].track_bpm - 80.0).abs() < 0.01, "{}", c.snap.decks[0].track_bpm);
        // Kept as the detected grid: the next load plays at 80.
        let o = c.trackdb.tracks.get(&TrackDb::key(&path)).expect("kept");
        assert!(o.grid_detected && o.grid.is_some_and(|g| (g.bpm - 80.0).abs() < 0.01 && g.anchor_secs.abs() < 0.01), "{o:?}");
        // Loaded again: at 80, and the heard beats find nothing to correct.
        c.load_track(0, &path);
        while c.decks[0].info.as_ref().is_none_or(|i| i.track_id == id) || !c.snap.decks[0].loaded {
            assert!(Instant::now() < deadline, "track never loaded again");
            pump(&mut e, &mut c);
        }
        assert!((c.snap.decks[0].track_bpm - 80.0).abs() < 0.01, "{}", c.snap.decks[0].track_bpm);
        c.status.clear();
        let id = c.decks[0].info.as_ref().unwrap().track_id;
        c.on_stems_update(Update::Sections { deck: 0, track_id: id, found, cues: None, drums: None });
        assert!(!c.status.iter().any(|(_, m)| m.contains("→") || m.contains("corrected")), "{:?}", c.status);
    }

    #[test]
    fn the_drum_stem_settles_a_bpm_the_heard_beats_cannot() {
        use crate::stems::{Beat, Structure, Update};
        let (mut e, mut c, _config) = core();
        c.load_track(0, &kick_wav("drum-bpm.wav", 40));
        let deadline = Instant::now() + std::time::Duration::from_secs(60);
        while !(c.snap.decks[0].loaded && c.decks[0].info.is_some()) {
            assert!(Instant::now() < deadline, "track never loaded");
            pump(&mut e, &mut c);
        }
        let id = c.decks[0].info.as_ref().unwrap().track_id;
        // The model's beats wander (every third interval 20 % long); the drum stem keeps 150.
        let mut t = 0.0;
        let beats: Vec<Beat> = (0..60)
            .map(|k| {
                t += if k % 3 == 0 { 0.48 } else { 0.4 };
                Beat { secs: t, downbeat: 0.1 }
            })
            .collect();
        let found = Arc::new(Structure { sections: vec![], beats });
        // Without the drums' grid: nothing to go on.
        c.on_stems_update(Update::Sections { deck: 0, track_id: id, found: found.clone(), cues: None, drums: None });
        assert!(!c.status.iter().any(|(_, m)| m.contains("→")), "{:?}", c.status);
        let drums = dj_engine::track::BeatGrid { bpm: 150.0, anchor_secs: 0.0 };
        c.on_stems_update(Update::Sections { deck: 0, track_id: id, found, cues: None, drums: Some(drums) });
        assert!(c.status.iter().any(|(_, m)| m.contains("BPM 120.0 → 150.0, from the drum stem")), "{:?}", c.status);
        pump(&mut e, &mut c);
        assert!((c.snap.decks[0].track_bpm - 150.0).abs() < 0.01, "{}", c.snap.decks[0].track_bpm);
    }

    #[test]
    fn fits_follows_the_deck_you_hear_not_a_silent_master() {
        let (mut e, mut c, _config) = core();
        for (d, name) in [(0, "ref-a.wav"), (1, "ref-b.wav"), (2, "ref-c.wav")] {
            c.load_track(d, &kick_wav(name, 30));
            let deadline = Instant::now() + std::time::Duration::from_secs(60);
            while !(c.snap.decks[d].loaded && c.decks[d].info.is_some()) {
                assert!(Instant::now() < deadline, "track never loaded");
                pump(&mut e, &mut c);
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        assert_eq!(c.reference_deck(), None, "nothing playing");
        // Deck 1 plays first (the master), then deck 3; deck 1's fader goes down (looping in
        // your headphones, say), deck 3's is full.
        c.deck_cmd(0, DeckCmd::Play);
        for _ in 0..20 {
            pump(&mut e, &mut c);
        }
        c.deck_cmd(2, DeckCmd::Play);
        c.route("Mixer/CH1/Volume", &[OscType::Float(0.0)]);
        c.route("Mixer/CH2/Volume", &[OscType::Float(0.0)]);
        c.route("Mixer/CH3/Volume", &[OscType::Float(1.0)]);
        for _ in 0..20 {
            pump(&mut e, &mut c);
        }
        assert_eq!(c.snap.clock.owner, ClockOwner::Deck(0), "deck 1 is still the master");
        assert_eq!(c.reference_deck(), Some(2), "but deck 3 is what you hear");
        // Its fader back up: the master again.
        c.route("Mixer/CH1/Volume", &[OscType::Float(0.8)]);
        assert_eq!(c.reference_deck(), Some(0));
        // Muted counts as down; with nothing up at all, the master.
        c.route("Mixer/CH1/Mute", &[OscType::Int(1)]);
        assert_eq!(c.reference_deck(), Some(2));
        c.route("Mixer/CH3/Volume", &[OscType::Float(0.0)]);
        assert_eq!(c.reference_deck(), Some(0));
    }

    #[test]
    fn a_mix_waits_for_the_engine_to_start_the_new_deck() {
        let (mut e, mut c, _config) = core();
        for (d, name) in [(0, "race-a.wav"), (1, "race-b.wav")] {
            c.load_track(d, &kick_wav(name, 60));
            let deadline = Instant::now() + std::time::Duration::from_secs(60);
            while !(c.snap.decks[d].loaded && c.decks[d].info.is_some()) {
                assert!(Instant::now() < deadline, "track never loaded");
                pump(&mut e, &mut c);
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        c.deck_cmd(0, DeckCmd::Play);
        for _ in 0..20 {
            pump(&mut e, &mut c);
        }
        let plan = c.plan_now(1, 0, crate::settings::MixStyle::Eq);
        c.start_mix(plan);
        // The screen redraws before the engine has played a block: the new deck isn't playing
        // (nor waiting to) in what it shows yet. That's not a deck stopped under the mix.
        for _ in 0..3 {
            c.tick();
            assert!(c.automix.is_some(), "cancelled before the engine started the deck: {:?}", c.status);
        }
        pump(&mut e, &mut c);
        assert!(c.snap.decks[1].playing || c.snap.decks[1].play_pending);
        assert!(c.automix.is_some());
        // Stopped for real (after it was seen playing): the mix gives up.
        c.deck_cmd(1, DeckCmd::Pause);
        pump(&mut e, &mut c);
        pump(&mut e, &mut c);
        assert!(c.automix.is_none(), "the new deck stopped: cancelled");
    }

    #[test]
    fn a_tap_right_after_a_mix_starts_does_not_cancel_it() {
        let (mut e, mut c, _config) = core();
        for (d, name) in [(0, "tap-a.wav"), (1, "tap-b.wav")] {
            c.load_track(d, &kick_wav(name, 60));
            let deadline = Instant::now() + std::time::Duration::from_secs(60);
            while !(c.snap.decks[d].loaded && c.decks[d].info.is_some()) {
                assert!(Instant::now() < deadline, "track never loaded");
                pump(&mut e, &mut c);
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        c.deck_cmd(0, DeckCmd::Play);
        for _ in 0..20 {
            pump(&mut e, &mut c);
        }
        // In the old track's last minute, one tap on Mix starts it: the second tap of a double
        // tap lands on the same button, now the mix's.
        assert!(c.mix_is_urgent(0));
        let plan = c.plan_best(1, 0, c.settings.mix_style);
        c.start_mix(plan);
        pump(&mut e, &mut c);
        assert!(!c.tap_cancel_mix(), "too soon to be meant");
        assert!(c.automix.is_some());
        std::thread::sleep(std::time::Duration::from_millis(1600));
        assert!(c.tap_cancel_mix());
        assert!(c.automix.is_none());
        // Not brought in yet: it says the new track still waits, silent.
        assert!(c.status.iter().any(|(_, m)| m == "Mix cancelled: deck 2 still waits, volume down"), "{:?}", c.status);
    }

    #[test]
    fn mix_now_is_over_in_about_half_a_minute() {
        // Two long tracks that fit everywhere: Best's search would blend them for 44 bars (90 s),
        // little happening for the first minute. Now keeps it to 16 bars (32 s at 120).
        let (mut e, mut c, _config) = core();
        for (d, name) in [(0, "now-a.wav"), (1, "now-b.wav")] {
            c.load_track(d, &kick_wav(name, 180));
            let deadline = Instant::now() + std::time::Duration::from_secs(60);
            while !(c.snap.decks[d].loaded && c.decks[d].info.is_some()) {
                assert!(Instant::now() < deadline, "track never loaded");
                pump(&mut e, &mut c);
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        c.deck_cmd(0, DeckCmd::Play);
        while c.snap.decks[0].position_secs < 20.0 {
            pump(&mut e, &mut c);
        }
        for style in [crate::settings::MixStyle::Auto, crate::settings::MixStyle::Eq, crate::settings::MixStyle::Stems] {
            let plan = c.plan_now(1, 0, style);
            assert_eq!(plan.length_beats, 64.0, "{style:?}: {plan:?}");
            assert!(plan.start_beat - c.snap.decks[0].beat <= 24.0, "starts within a few bars: {plan:?}");
        }
    }

    #[test]
    fn mix_now_brings_the_looping_deck_in_and_stops_the_other() {
        let (mut e, mut c, _config) = core();
        // Deck 2 first, so it becomes the tempo master (the mix must hand that over).
        for (d, name, secs) in [(1, "automix-b.wav", 60), (0, "automix-a.wav", 90)] {
            c.load_track(d, &kick_wav(name, secs));
            let deadline = Instant::now() + std::time::Duration::from_secs(60);
            while !(c.snap.decks[d].loaded && c.decks[d].info.is_some()) {
                assert!(Instant::now() < deadline, "track never loaded");
                pump(&mut e, &mut c);
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        for d in 0..2 {
            c.deck_cmd(d, DeckCmd::SetGrid(Some(dj_engine::track::BeatGrid { bpm: 120.0, anchor_secs: 0.0 })));
        }
        c.route("Mixer/CH1/Volume", &[OscType::Float(0.9)]);
        // Deck 2 is silent by mute (fader up): the mix must unmute it and fade it in from 0.
        c.route("Mixer/CH2/Volume", &[OscType::Float(0.6)]);
        c.route("Mixer/CH2/Mute", &[OscType::Int(1)]);
        c.deck_cmd(0, DeckCmd::Play);
        // Deck 2 waits silently in a 4-beat loop at a cue 8 s in.
        c.deck_cmd(1, DeckCmd::SetCue { idx: 0, secs: Some(8.0), end_secs: None });
        c.deck_cmd(1, DeckCmd::CueBeatLoop { idx: 0, beats: 4.0 });
        // (Both start on the next beat.)
        for _ in 0..100 {
            pump(&mut e, &mut c);
        }
        // Deck 2's stems are ready, so Now can take its vocals out.
        let deadline = Instant::now() + std::time::Duration::from_secs(60);
        while !c.snap.decks[1].stems_ready {
            assert!(Instant::now() < deadline, "stems never attached");
            pump(&mut e, &mut c);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(c.snap.clock.owner, ClockOwner::Deck(1));
        assert_eq!(c.mix_target(1), Some(0));
        assert_eq!(c.mix_target(0), None, "the playing deck is not a candidate");
        assert!(!c.mix_is_urgent(0));

        // Best: searched over the whole track: on a 4-bar line, ahead, whole phrases long.
        let best = c.plan_best(1, 0, crate::settings::MixStyle::Eq);
        let beat = c.snap.decks[0].beat;
        assert_eq!(best.mode, automix::MixWhen::Best);
        assert!(best.length_beats >= 16.0 && best.length_beats % 16.0 == 0.0, "{best:?}");
        assert_eq!(best.start_beat % 16.0, 0.0);
        assert!(best.start_beat > beat, "{best:?}");

        let plan = c.plan_now(1, 0, crate::settings::MixStyle::Eq);
        // Now: on the next 4-bar line or so, as long as the music allows up to about 30 s (16
        // bars at 120: not the long blend Best may pick, in which little happens for a minute).
        assert!(plan.length_beats >= 16.0 && plan.length_beats <= 64.0 && plan.length_beats % 16.0 == 0.0, "{plan:?}");
        assert_eq!(plan.start_beat % 16.0, 0.0);
        assert!(plan.start_beat > c.snap.decks[0].beat);
        let start = plan.start_beat;
        let len = plan.length_beats;
        let ps = plan.blend.as_ref().map_or(0.5, |b| b.swap_fraction());
        c.start_mix(plan);
        pump(&mut e, &mut c);
        assert_eq!(c.snap.clock.owner, ClockOwner::Deck(0));
        assert!(c.snap.decks[1].sync);
        assert!(!c.session.channels[1].params.mute && c.session.channels[1].params.volume == 0.0, "unmuted at volume 0");
        assert!(c.mix_target(1).is_none(), "one mix at a time");

        let low = |c: &Core, ch: usize| c.session.channels[ch].params.eq[0];
        let vol = |c: &Core, ch: usize| c.session.channels[ch].params.volume;
        let (mut saw_in_bass_out, mut saw_swap) = (false, false);
        let mut blocks = 0;
        while c.automix.is_some() {
            pump(&mut e, &mut c);
            blocks += 4;
            assert!(blocks < 48000 / 512 * 300, "mix never finished");
            let b = c.snap.decks[0].beat;
            let p = (b - start) / len;
            if c.snap.decks[0].playing && p > 0.05 && p < ps - 0.05 {
                // Before the swap: incoming coming up without bass, outgoing at full volume.
                assert!(c.snap.decks[1].playing && !c.snap.decks[1].loop_on, "incoming left its loop");
                assert!(c.snap.decks[1].sync);
                assert_eq!(low(&c, 1), 0.0);
                assert!((vol(&c, 0) - 0.9).abs() < 1e-6);
                saw_in_bass_out = vol(&c, 1) > 0.05;
            }
            if c.snap.decks[0].playing && p > ps + 0.05 && p < 0.95 {
                assert_eq!(low(&c, 0), 0.0, "basses swapped where the plan says");
                assert!((low(&c, 1) - 0.5).abs() < 1e-6);
                saw_swap = true;
            }
        }
        assert!(saw_in_bass_out && saw_swap);
        pump(&mut e, &mut c);
        assert!(!c.snap.decks[0].playing, "outgoing stopped");
        assert_eq!(c.snap.clock.owner, ClockOwner::Deck(1), "the new track leads");
        assert!(c.snap.decks[1].playing);
        assert_eq!(vol(&c, 0), 0.0);
        assert!((vol(&c, 1) - 1.0).abs() < 1e-6);
        assert!(!c.session.channels[1].params.mute);
        assert!((low(&c, 0) - 0.5).abs() < 1e-6 && (low(&c, 1) - 0.5).abs() < 1e-6);
        assert_eq!((c.session.channels[0].params.eq[1], c.session.channels[0].params.eq[2]), (0.5, 0.5), "old mids and highs back");
        // The incoming track played on through the mix (0.5 s a beat), from where the plan
        // brought it in.
        assert!(c.snap.decks[1].position_secs > len * 0.5 - 0.3, "{}", c.snap.decks[1].position_secs);
        assert_eq!(c.snap.decks[1].stem_gains[2], 1.0, "no old vocals: the new track's are in");
        // Until another track is loaded on that deck: it starts with every stem on.
        c.load_track(1, &kick_wav("automix-c.wav", 30));
        let deadline = Instant::now() + std::time::Duration::from_secs(60);
        while c.decks[1].info.as_ref().is_none_or(|i| !i.path.ends_with("automix-c.wav")) || !c.snap.decks[1].loaded {
            assert!(Instant::now() < deadline, "track never loaded");
            pump(&mut e, &mut c);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        pump(&mut e, &mut c);
        assert_eq!(c.snap.decks[1].stem_gains, [1.0; 4], "stems reset on load");
        assert!(c.pickup_allows("Deck/2/StemVocalsLevel", 0.3), "no pick-up left from the old track");

        // MIDI pick-up: the controller's CH1 fader (still up) is ignored until it comes down to
        // dj's 0, then follows again.
        assert!(!c.pickup_allows("/Mixer/CH1/Volume", 0.9));
        assert!(!c.pickup_allows("/Mixer/CH1/Volume", 0.5));
        assert!(c.pickup_allows("/Mixer/CH1/Volume", 0.01));
        assert!(c.pickup_allows("/Mixer/CH1/Volume", 0.7));
        // Crossing the current value counts too (a fast move skips past it).
        c.route("Mixer/CH2/Volume", &[OscType::Float(0.5)]);
        c.pickup.insert("mixer/ch2/volume".into(), None);
        assert!(!c.pickup_allows("Mixer/CH2/Volume", 0.2));
        assert!(c.pickup_allows("Mixer/CH2/Volume", 0.8), "jumped across 0.5");
    }

    #[test]
    fn hot_cue_pads_loop_or_jump_the_same_on_screen_and_midi() {
        let (mut e, mut c, _config) = core();
        c.load_track(0, &kick_wav("pads.wav", 30));
        let deadline = Instant::now() + std::time::Duration::from_secs(60);
        while !(c.snap.decks[0].loaded && c.decks[0].info.is_some()) {
            assert!(Instant::now() < deadline, "track never loaded");
            pump(&mut e, &mut c);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        c.deck_cmd(0, DeckCmd::SetGrid(Some(dj_engine::track::BeatGrid { bpm: 120.0, anchor_secs: 0.0 })));
        c.route("Header/QuantizeOn", &[OscType::Int(0)]);
        c.deck_cmd(0, DeckCmd::SetCue { idx: 0, secs: Some(4.0), end_secs: None });
        c.deck_cmd(0, DeckCmd::Play);
        for _ in 0..30 {
            pump(&mut e, &mut c);
        }
        assert!(c.snap.decks[0].playing);

        // Loop mode (default), at the deck's loop size, which the loop buttons set.
        assert!(c.route("Deck/1/LoopSize", &[OscType::Float(8.0)]));
        assert_eq!(c.loop_size[0], 8.0);
        assert!(c.route("Deck/1/Cue1", &[OscType::Int(1)]), "a pad press, as from the K2");
        for _ in 0..4 {
            pump(&mut e, &mut c);
        }
        let v = c.snap.decks[0];
        assert!(v.loop_on && v.loop_beats == 8.0 && (v.loop_start_secs - 4.0).abs() < 0.01, "{} {} {}", v.loop_on, v.loop_beats, v.loop_start_secs);

        // Jump mode: the same press jumps to the cue and plays on, no loop.
        c.route("Deck/1/LoopExit", &[OscType::Int(1)]);
        c.settings.cue_pads = crate::settings::CuePadMode::Jump;
        c.route("Deck/1/Cue1", &[OscType::Int(1)]);
        for _ in 0..4 {
            pump(&mut e, &mut c);
        }
        let v = c.snap.decks[0];
        assert!(!v.loop_on && v.playing && v.position_secs >= 4.0 && v.position_secs < 4.5, "{} {}", v.loop_on, v.position_secs);

        // An empty pad sets a cue in either mode.
        c.route("Deck/1/Cue2", &[OscType::Int(1)]);
        for _ in 0..2 {
            pump(&mut e, &mut c);
        }
        assert!(c.snap.decks[0].hot_cues[1].is_some());
    }

    #[test]
    fn group_buttons_are_picked_up_including_tempo_aliases() {
        let (mut e, mut c, _config) = core();
        // The fader popup's Volume / EQ groups write through pick-up.
        c.route("Mixer/CH1/Volume", &[OscType::Float(0.8)]);
        c.set_picked_up("Mixer/CH1/Volume", 0.0);
        assert!(!c.pickup_allows("/Mixer/CH1/Volume", 0.8), "fader still up: ignored");
        assert_eq!(c.session.channels[0].params.volume, 0.0);
        assert!(c.pickup_allows("/Mixer/CH1/Volume", 0.02), "reached: follows again");
        // Tempo reset writes Deck/N/Tempo (0..1); a controller on TempoSlider (-1..1) is the
        // same control.
        c.set_picked_up("Deck/2/Tempo", 0.5);
        pump(&mut e, &mut c);
        assert!(!c.pickup_allows("Deck/2/TempoSlider", 0.9));
        assert!(c.pickup_allows("Deck/2/TempoSlider", 0.4), "crossed 0 %");
        assert!(c.pickup_allows("Deck/2/TempoSlider", 0.9), "then free");
    }

    #[test]
    fn controller_tempo_moves_in_the_tempo_step_setting() {
        let (mut e, mut c, _config) = core();
        c.load_track(0, &kick_wav("steps.wav", 20));
        let deadline = Instant::now() + std::time::Duration::from_secs(60);
        while !(c.snap.decks[0].loaded && c.decks[0].info.is_some()) {
            assert!(Instant::now() < deadline, "track never loaded");
            pump(&mut e, &mut c);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        c.route("Deck/1/TempoRange", &[OscType::Int(1)]); // ±10 %
        c.settings.tempo_step_pct = 1.0;
        c.profile.mappings = vec![MidiMapping {
            source: MidiSource { kind: MidiKind::Cc, channel: 0, number: 7 },
            address: "Deck/1/TempoSlider".into(),
            mode: MapMode::Absolute,
            invert: false,
            feedback: false,
        }];
        // CC 70 of 127 is +1.0236 of ±10 % = +1.02 %: snapped to +1 %.
        c.handle_midi(&[0xb0, 7, 70]);
        pump(&mut e, &mut c);
        let pct = (c.snap.decks[0].speed / c.snap.decks[0].speed_mult - 1.0) * 100.0;
        assert!((pct - 1.0).abs() < 1e-6, "{pct}");
    }

    #[test]
    fn mix_with_an_external_mixer_only_drops_decks_in_and_out_of_loops() {
        let (mut e, mut c, _config) = core();
        c.audio_channels = 4; // two stereo pairs: external routing works
        c.set_external_mixer(true);
        assert!(c.external_routing());
        for (d, name, secs) in [(1, "ext-b.wav", 60), (0, "ext-a.wav", 90)] {
            c.load_track(d, &kick_wav(name, secs));
            let deadline = Instant::now() + std::time::Duration::from_secs(60);
            while !(c.snap.decks[d].loaded && c.decks[d].info.is_some()) {
                assert!(Instant::now() < deadline, "track never loaded");
                pump(&mut e, &mut c);
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        for d in 0..2 {
            c.deck_cmd(d, DeckCmd::SetGrid(Some(dj_engine::track::BeatGrid { bpm: 120.0, anchor_secs: 0.0 })));
        }
        // dj's own volumes mean nothing here (both up): the looping deck is still a candidate.
        c.route("Mixer/CH1/Volume", &[OscType::Float(0.7)]);
        c.route("Mixer/CH2/Volume", &[OscType::Float(0.6)]);
        c.deck_cmd(0, DeckCmd::Play);
        c.deck_cmd(1, DeckCmd::SetCue { idx: 0, secs: Some(8.0), end_secs: None });
        c.deck_cmd(1, DeckCmd::CueBeatLoop { idx: 0, beats: 4.0 });
        for _ in 0..100 {
            pump(&mut e, &mut c);
        }
        let deadline = Instant::now() + std::time::Duration::from_secs(60);
        while !c.snap.decks[1].stems_ready {
            assert!(Instant::now() < deadline, "stems never attached");
            pump(&mut e, &mut c);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(c.mix_target(1), Some(0));
        let stems = c.snap.decks[1].stems_ready;
        let plan = c.plan_now(1, 0, crate::settings::MixStyle::Eq);
        c.start_mix(plan);
        let mut blocks = 0;
        while c.automix.is_some() {
            pump(&mut e, &mut c);
            blocks += 4;
            assert!(blocks < 48000 / 512 * 300, "mix never finished");
        }
        pump(&mut e, &mut c);
        let (a, b) = (c.snap.decks[0], c.snap.decks[1]);
        assert!(b.playing && !b.loop_on && b.sync, "the new deck dropped in, in sync");
        assert!(a.playing && a.loop_on, "the old deck waits in a loop for your fade");
        let p = |ch: usize| c.session.channels[ch].params;
        assert_eq!((p(0).volume, p(1).volume), (0.7, 0.6), "volumes untouched");
        assert_eq!((p(0).eq[0], p(1).eq[0]), (0.5, 0.5), "no bass swap");
        assert!(stems);
        assert_eq!(c.snap.decks[1].stem_gains[2], 1.0, "no old vocals: the new track's are in");
    }

    /// Deck 1 plays (volume 0.9); deck 2 waits silently in a loop at its cue (8 s), with a second
    /// cue at 16 s; both have stems.
    fn mix_setup(e: &mut dj_engine::Engine, c: &mut Core) {
        // Deck 1 first: it is the tempo master.
        for (d, name, secs) in [(0, "style-a.wav", 90), (1, "style-b.wav", 60)] {
            c.load_track(d, &kick_wav(name, secs));
            let deadline = Instant::now() + std::time::Duration::from_secs(60);
            while !(c.snap.decks[d].loaded && c.decks[d].info.is_some()) {
                assert!(Instant::now() < deadline, "track never loaded");
                pump(e, c);
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        for d in 0..2 {
            c.deck_cmd(d, DeckCmd::SetGrid(Some(dj_engine::track::BeatGrid { bpm: 120.0, anchor_secs: 0.0 })));
        }
        c.route("Mixer/CH1/Volume", &[OscType::Float(0.9)]);
        c.route("Mixer/CH2/Volume", &[OscType::Float(0.0)]);
        c.deck_cmd(0, DeckCmd::Play);
        c.deck_cmd(1, DeckCmd::SetCue { idx: 0, secs: Some(8.0), end_secs: None });
        c.deck_cmd(1, DeckCmd::SetCue { idx: 1, secs: Some(16.0), end_secs: None });
        c.deck_cmd(1, DeckCmd::CueBeatLoop { idx: 0, beats: 4.0 });
        let deadline = Instant::now() + std::time::Duration::from_secs(60);
        // (Also until dj's cue detection is done, so it doesn't replace the cues tests set.)
        let pending = |c: &Core, d: usize| c.decks[d].info.as_ref().is_none_or(|i| i.cues_pending);
        while !(c.snap.decks[0].stems_ready && c.snap.decks[1].stems_ready && c.snap.decks[0].playing && c.snap.decks[1].loop_on)
            || pending(c, 0)
            || pending(c, 1)
        {
            assert!(Instant::now() < deadline, "decks never ready");
            pump(e, c);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(c.mix_target(1), Some(0));
    }

    #[test]
    fn a_mix_starts_the_new_track_from_the_cue_you_chose() {
        let (mut e, mut c, _config) = core();
        mix_setup(&mut e, &mut c);
        // Hot cue 2 (16 s) pressed on the waiting deck: that's where it comes in, whatever the
        // search would rather pick, blend or cut.
        assert!(c.route("Deck/2/Cue2", &[OscType::Int(1)]));
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        while (c.snap.decks[1].position_secs - 16.0).abs() > 0.5 && (c.snap.decks[1].loop_start_secs - 16.0).abs() > 0.01 {
            assert!(Instant::now() < deadline, "never went to the cue");
            pump(&mut e, &mut c);
        }
        assert!(c.decks[1].cue_chosen);
        for style in [crate::settings::MixStyle::Auto, crate::settings::MixStyle::Stems, crate::settings::MixStyle::Drop] {
            for plan in [c.plan_best(1, 0, style), c.plan_now(1, 0, style)] {
                let from = plan.entry_secs.unwrap_or(if c.snap.decks[1].loop_on { c.snap.decks[1].loop_start_secs } else { c.snap.decks[1].position_secs });
                assert!((from - 16.0).abs() < 0.01, "{style:?}: from {from} s: {plan:?}");
            }
        }
        // A new track: nothing chosen yet.
        c.load_track(1, &kick_wav("style-b.wav", 60));
        assert!(!c.decks[1].cue_chosen);
    }

    fn run_mix(e: &mut dj_engine::Engine, c: &mut Core, mut each: impl FnMut(&Core, f64)) {
        let (start, len) = c.automix.as_ref().map(|m| (m.plan.start_beat, m.plan.length_beats)).unwrap();
        let mut blocks = 0;
        while c.automix.is_some() {
            pump(e, c);
            blocks += 4;
            assert!(blocks < 48000 / 512 * 300, "mix never finished");
            if c.snap.decks[0].playing {
                each(c, (c.snap.decks[0].beat - start) / len);
            }
        }
        pump(e, c);
    }

    #[test]
    fn vocals_held_out_of_a_mix_come_back_even_without_a_vocal_phrase_to_wait_for() {
        let (mut e, mut c, _config) = core();
        mix_setup(&mut e, &mut c);
        // A plan that keeps the new vocals out to its end, and no vocal waveform for the new
        // track (not computed yet, or no phrase that clears): nothing to wait for.
        let mut plan = c.plan_now(1, 0, crate::settings::MixStyle::Stems);
        let b = plan.blend.as_mut().expect("a blend");
        b.in_at[2] = Some(b.bars as f64);
        c.decks[1].stem_wave = None;
        c.start_mix(plan);
        run_mix(&mut e, &mut c, |_, _| {});
        assert_eq!(c.snap.decks[1].stem_gains[2], 0.0, "held out to the end of the mix");
        // Back over a bar: the new track all in, not left without its vocals.
        for _ in 0..200 {
            pump(&mut e, &mut c);
        }
        assert_eq!(c.snap.decks[1].stem_gains, [1.0; 4]);
        assert!(c.vocal_return.is_none());
    }

    #[test]
    fn after_a_mix_the_new_track_eases_back_to_its_own_tempo() {
        let (mut e, mut c, _config) = core();
        mix_setup(&mut e, &mut c);
        // The new track is 1.25 % faster than the playing one: the mix plays it at 120.
        c.deck_cmd(1, DeckCmd::SetGrid(Some(dj_engine::track::BeatGrid { bpm: 121.5, anchor_secs: 0.0 })));
        pump(&mut e, &mut c);
        let plan = c.plan_now(1, 0, crate::settings::MixStyle::Eq);
        c.start_mix(plan);
        run_mix(&mut e, &mut c, |_, _| {});
        assert!(c.tempo_home.is_some(), "it eases home");
        assert!((c.snap.decks[1].bpm - 120.0).abs() < 0.05, "the mix ended at the old tempo: {}", c.snap.decks[1].bpm);
        // Then 0.25 % a bar at most, until it's at its own tempo (5 bars).
        let (mut last, mut beat) = (c.snap.decks[1].bpm, c.snap.clock.beat);
        let start = beat;
        while c.tempo_home.is_some() {
            pump(&mut e, &mut c);
            let (bpm, b) = (c.snap.decks[1].bpm, c.snap.clock.beat);
            assert!(bpm >= last - 1e-9, "only towards its own tempo");
            assert!(bpm - last <= 121.5 * 0.0025 * ((b - beat) / 4.0).max(0.25) + 0.01, "{last} → {bpm} in {} beats", b - beat);
            (last, beat) = (bpm, b);
            assert!(b - start < 64.0, "never got home");
        }
        pump(&mut e, &mut c);
        assert!((c.snap.decks[1].bpm - 121.5).abs() < 0.01 && c.snap.decks[1].tempo_fader == 0.0, "{}", c.snap.decks[1].bpm);
        assert!(c.status.iter().any(|(_, m)| m.contains("back at its own tempo")));
        assert!(c.snap.clock.beat - start > 16.0, "slowly: {} beats", c.snap.clock.beat - start);

        // Moving its tempo fader stops it.
        c.tempo_home = Some(automix::TempoHome::new(1, c.snap.decks[1].track_id));
        c.deck_cmd(1, DeckCmd::Tempo(0.3));
        for _ in 0..20 {
            pump(&mut e, &mut c);
        }
        c.deck_cmd(1, DeckCmd::Tempo(0.25));
        for _ in 0..20 {
            pump(&mut e, &mut c);
        }
        assert!(c.tempo_home.is_none(), "yours from there");
        assert!((c.snap.decks[1].tempo_fader - 0.25).abs() < 1e-9);
    }

    #[test]
    fn after_a_mix_the_transposed_track_goes_back_to_its_own_key() {
        let (mut e, mut c, _config) = core();
        mix_setup(&mut e, &mut c);
        let mut plan = c.plan_now(1, 0, crate::settings::MixStyle::Eq);
        plan.transpose = 1;
        c.start_mix(plan);
        pump(&mut e, &mut c);
        assert_eq!(c.snap.decks[1].semitones, 1.0, "shifted to fit for the mix");
        run_mix(&mut e, &mut c, |_, _| {});
        let kh = c.key_home.expect("it goes home");
        assert_eq!(c.snap.decks[1].semitones, 1.0, "still shifted when the mix ends");
        assert!(kh.at % 32.0 == 0.0 && kh.at >= c.snap.decks[1].beat + 4.0, "on a phrase line, a bar away: {kh:?}");
        // Back to its own key on that line, not before.
        while c.key_home.is_some() {
            assert_eq!(c.snap.decks[1].semitones, 1.0);
            pump(&mut e, &mut c);
            assert!(c.snap.decks[1].beat < kh.at + 4.0, "never went home");
        }
        pump(&mut e, &mut c);
        assert_eq!(c.snap.decks[1].semitones, 0.0);
        assert!(c.snap.decks[1].beat >= kh.at);
        assert!(c.status.iter().any(|(_, m)| m.contains("back in its own key")));

        // A key you set yourself meanwhile stays.
        c.key_home = Some(automix::KeyHome { at: c.snap.decks[1].beat + 8.0, from: 0.0, to: -1.0, ..kh });
        c.deck_cmd(1, DeckCmd::Semitones(2.0));
        for _ in 0..200 {
            pump(&mut e, &mut c);
        }
        assert!(c.key_home.is_none() && c.snap.decks[1].semitones == 2.0, "yours from there");
    }

    #[test]
    fn auto_assesses_the_tracks_and_stems_mix_swaps_the_parts() {
        use crate::core::automix::{MixHow, MixWhen};
        use crate::settings::MixStyle;
        let (mut e, mut c, _config) = core();
        mix_setup(&mut e, &mut c);

        // Same tempo, no keys, stems on both: Auto picks Stems.
        assert_eq!(c.assess(1, 0, MixWhen::Now).0, MixHow::Stems);
        // A drop cue on the new track: Auto drops for a Best mix (Now still blends).
        c.decks[1].info.as_mut().unwrap().cues[1] = Some(Cue { kind: CueKind::Hot, secs: 16.0, end_secs: None, name: "Drop".into(), color: [0; 3] });
        assert_eq!(c.assess(1, 0, MixWhen::Best), (MixHow::Drop, "the new track has a drop".to_string()));
        assert_eq!(c.assess(1, 0, MixWhen::Now).0, MixHow::Stems);
        // Tempos far apart: Drop.
        c.deck_cmd(1, DeckCmd::SetGrid(Some(dj_engine::track::BeatGrid { bpm: 140.0, anchor_secs: 0.0 })));
        pump(&mut e, &mut c);
        assert_eq!(c.assess(1, 0, MixWhen::Now).0, MixHow::Drop);
        c.deck_cmd(1, DeckCmd::SetGrid(Some(dj_engine::track::BeatGrid { bpm: 120.0, anchor_secs: 0.0 })));
        pump(&mut e, &mut c);
        // Stems asked for but missing on a deck: EQ.
        let plan = c.plan_now(1, 0, MixStyle::Stems);
        assert_eq!(plan.how, MixHow::Stems);

        // The plan comes from both tracks' stems; the moves follow it.
        let blend = plan.blend.clone().expect("both decks have stems: a planned blend");
        let ps = blend.swap_fraction();
        c.start_mix(plan);
        let (mut before, mut after) = (false, false);
        run_mix(&mut e, &mut c, |c, p| {
            let (gi, go) = (c.snap.decks[1].stem_gains, c.snap.decks[0].stem_gains);
            if (0.05..ps - 0.03).contains(&p) {
                assert_eq!((gi[1], go[1]), (0.0, 1.0), "before the swap: new bass out, old bass in");
                if p < 0.3 {
                    assert!(c.session.channels[1].params.eq[2] < 0.5, "new highs still rising");
                }
                before = true;
            }
            if (ps + 0.03..(ps + 0.12).min(0.9)).contains(&p) {
                assert_eq!((gi[1], go[1]), (1.0, 0.0), "the basses swapped where the plan says");
                assert_eq!(c.session.channels[0].params.volume, 0.9, "the stems do the fading, not the volume");
                assert_eq!(c.session.channels[1].params.eq[2], 0.5, "new highs in");
                after = true;
            }
        });
        assert!(before && after);
        let p = |ch: usize| c.session.channels[ch].params;
        assert!(!c.snap.decks[0].playing && c.snap.decks[1].playing);
        assert_eq!((p(0).volume, p(1).volume), (0.0, 1.0));
        assert_eq!((p(0).eq[0], p(1).eq[0]), (0.5, 0.5), "the bass swaps on the stems, not the EQ");
        assert_eq!((p(0).eq[2], p(1).eq[2]), (0.5, 0.5), "highs back to where they were");
        assert_eq!(c.snap.decks[0].stem_gains, [1.0; 4], "the old deck's stems back for next time");
        assert_eq!(c.snap.decks[1].stem_gains, [1.0; 4], "no old vocals: the new ones are in");
    }

    #[test]
    fn drop_mix_cuts_in_from_the_drop_cue() {
        use crate::core::automix::MixHow;
        let (mut e, mut c, _config) = core();
        mix_setup(&mut e, &mut c);
        c.decks[1].info.as_mut().unwrap().cues[1] = Some(Cue { kind: CueKind::Hot, secs: 16.0, end_secs: None, name: "Drop 1".into(), color: [0; 3] });
        c.deck_cmd(1, DeckCmd::SetCue { idx: 1, secs: Some(16.0), end_secs: None });
        pump(&mut e, &mut c);
        let plan = c.plan_now(1, 0, crate::settings::MixStyle::Drop);
        assert_eq!((plan.how, plan.length_beats), (MixHow::Drop, 4.0));
        assert_eq!(plan.entry_secs, Some(16.0), "cuts in on its drop cue: {plan:?}");
        let start = plan.start_beat;
        c.start_mix(plan);
        run_mix(&mut e, &mut c, |c, p| {
            if p > 0.3 {
                assert_eq!(c.session.channels[1].params.volume, 1.0, "cut in at full");
                assert!(c.session.channels[0].params.volume < 0.01, "old track gone within a beat");
            }
        });
        let b = c.snap.decks[1];
        assert!(!c.snap.decks[0].playing && b.playing && !b.loop_on);
        // From the drop: 16 s plus the bar of the mix (and a little).
        assert!((18.0..19.0).contains(&b.position_secs), "{} (start beat {start})", b.position_secs);
    }

    #[test]
    fn a_mashup_brings_the_new_vocals_over_the_old_music() {
        use crate::settings::MixStyle;
        let (mut e, mut c, _config) = core();
        mix_setup(&mut e, &mut c);
        // (The test model's vocals are silent.) No singing to bring: as Auto, saying why.
        let p = c.plan_now(1, 0, MixStyle::Mashup);
        assert!(p.why_how.starts_with("no mashup: the new track has no vocals"), "{}", p.why_how);
        // The new track sings throughout.
        let w = c.decks[1].stem_wave.clone().expect("stems");
        let sung = crate::stems::StemWave { bins_per_sec: w.bins_per_sec, levels: [w.levels[0].clone(), w.levels[1].clone(), vec![0.5; w.levels[2].len()], w.levels[3].clone()] };
        c.decks[1].stem_wave = Some(Arc::new(sung));
        let plan = c.plan_now(1, 0, MixStyle::Mashup);
        assert_eq!(plan.why_how, "mashup");
        let b = plan.blend.clone().expect("a stems plan");
        let bars = b.swap;
        assert!(b.vocals_first && bars >= 8.0 && bars % 8.0 == 0.0, "{b:?}");
        assert_eq!(plan.length_beats / 4.0, bars + 2.0, "and two bars as the old music leaves");
        c.start_mix(plan);
        // Over the singing: the new vocals alone on the new deck, the old music without its vocals.
        let mut seen = 0;
        run_mix(&mut e, &mut c, |c, p| {
            let bp = p * (bars + 2.0);
            if (1.5..bars - 1.5).contains(&bp) {
                seen += 1;
                assert_eq!(c.snap.decks[1].stem_gains, [0.0, 0.0, 1.0, 0.0], "bar {bp:.1}");
                assert_eq!(c.snap.decks[0].stem_gains, [1.0, 1.0, 0.0, 1.0], "bar {bp:.1}");
            }
        });
        assert!(seen > 10, "{seen}");
        assert_eq!(c.snap.decks[1].stem_gains, [1.0; 4], "then all of the new track");
    }

    #[test]
    fn external_stems_mix_leaves_the_old_deck_silent_in_its_loop() {
        let (mut e, mut c, _config) = core();
        c.audio_channels = 4;
        c.set_external_mixer(true);
        mix_setup(&mut e, &mut c);
        let plan = c.plan_now(1, 0, crate::settings::MixStyle::Stems);
        c.start_mix(plan);
        run_mix(&mut e, &mut c, |_, _| {});
        // Until you've faded it out on the mixer: looping, every stem at 0, and it stays so.
        for _ in 0..200 {
            pump(&mut e, &mut c);
        }
        let a = c.snap.decks[0];
        assert!(a.playing && a.loop_on);
        assert_eq!(a.stem_gains, [0.0; 4], "the old deck's sound doesn't come back");
        assert_eq!(c.snap.decks[1].stem_gains, [1.0; 4], "no old vocals: the new ones are in");
        // Only a new track (or you) changes that.
        c.load_track(0, &kick_wav("style-c.wav", 30));
        let deadline = Instant::now() + std::time::Duration::from_secs(60);
        while c.decks[0].info.as_ref().is_none_or(|i| !i.path.ends_with("style-c.wav")) || !c.snap.decks[0].loaded {
            assert!(Instant::now() < deadline, "track never loaded");
            pump(&mut e, &mut c);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        pump(&mut e, &mut c);
        assert_eq!(c.snap.decks[0].stem_gains, [1.0; 4]);
    }

    #[test]
    fn imported_folders_include_every_depth_and_report_problems() {
        let (mut e, mut c, _config) = core();
        let root = crate::settings::config_dir().join("music");
        let deep = root.join("a/b/c/d");
        std::fs::create_dir_all(&deep).unwrap();
        let wav = kick_wav("deep.wav", 2);
        std::fs::copy(&wav, deep.join("deep.wav")).unwrap();
        std::fs::copy(&wav, root.join("top.wav")).unwrap();

        c.add_library_folder(root.join("missing"));
        assert!(c.settings.library_folders.is_empty(), "a folder that isn't there isn't added");
        assert!(c.status.back().is_some_and(|(_, m)| m.contains("not found")), "{:?}", c.status.back());

        c.add_library_folder(root.clone());
        assert_eq!(c.settings.library_folders, vec![root.clone()]);
        let deadline = Instant::now() + std::time::Duration::from_secs(60);
        while c.library_scanning {
            assert!(Instant::now() < deadline, "scan never finished");
            pump(&mut e, &mut c);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let mut names: Vec<String> = c.library.iter().map(|t| t.path.file_name().unwrap().to_string_lossy().into_owned()).collect();
        names.sort();
        assert_eq!(names, vec!["deep.wav", "top.wav"]);
        // Adding it again doesn't duplicate it.
        c.add_library_folder(root.clone());
        assert_eq!(c.settings.library_folders.len(), 1);
    }

    #[test]
    fn mix_brings_in_a_paused_deck() {
        let (mut e, mut c, _config) = core();
        for (d, name, secs) in [(0, "paused-a.wav", 90), (1, "paused-b.wav", 60)] {
            c.load_track(d, &kick_wav(name, secs));
            let deadline = Instant::now() + std::time::Duration::from_secs(60);
            while !(c.snap.decks[d].loaded && c.decks[d].info.as_ref().is_some_and(|i| !i.cues_pending)) {
                assert!(Instant::now() < deadline, "track never loaded");
                pump(&mut e, &mut c);
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        for d in 0..2 {
            c.deck_cmd(d, DeckCmd::SetGrid(Some(dj_engine::track::BeatGrid { bpm: 120.0, anchor_secs: 0.0 })));
        }
        c.route("Mixer/CH1/Volume", &[OscType::Float(0.9)]);
        // Deck 2 is paused with its fader up: Mix still offers it, and takes the volume down first.
        c.route("Mixer/CH2/Volume", &[OscType::Float(0.7)]);
        c.deck_cmd(0, DeckCmd::Play);
        for _ in 0..60 {
            pump(&mut e, &mut c);
        }
        assert!(c.snap.decks[0].playing && !c.snap.decks[1].playing);
        assert_eq!(c.mix_target(1), Some(0));
        let plan = c.plan_now(1, 0, crate::settings::MixStyle::Eq);
        c.start_mix(plan);
        assert_eq!(c.session.channels[1].params.volume, 0.0, "silent until the mix brings it in");
        let mut heard = false;
        // How DJs stage it (the DJ-mix studies' EQ curves): the old highs come down to make room
        // while the old bass still plays; the old bass is the last to go.
        let (mut room, mut highs_first) = (false, true);
        run_mix(&mut e, &mut c, |c, p| {
            if (0.1..0.4).contains(&p) {
                let b = c.snap.decks[1];
                assert!(b.playing && b.sync && !b.loop_on, "started in sync and left its loop");
                heard = c.session.channels[1].params.volume > 0.05;
            }
            let old = c.session.channels[0].params.eq;
            if old[0] == 0.5 && old[2] < 0.45 {
                room = true;
            }
            if old[0] < 0.5 && !room {
                highs_first = false;
            }
        });
        assert!(heard);
        assert!(room && highs_first, "the old highs make room before the old bass goes");
        assert!(!c.snap.decks[0].playing && c.snap.decks[1].playing);
        assert_eq!(c.session.channels[1].params.volume, 1.0);
        // In time: deck 2's beat lines up with the clock's.
        assert!(c.snap.decks[1].phase_error.abs() < 0.05, "{}", c.snap.decks[1].phase_error);
    }

    #[test]
    fn stems_are_made_once_then_reused() {
        let (mut e, mut c, _config) = core();
        let path = kick_wav("reuse.wav", 20);
        for (d, want) in [(0, "HT-Demucs"), (1, "prepared stems"), (0, "prepared stems")] {
            c.load_track(d, &path);
            let deadline = Instant::now() + std::time::Duration::from_secs(60);
            while !(c.snap.decks[d].stems_ready && c.decks[d].stems_source.is_some()) {
                assert!(Instant::now() < deadline, "stems never came");
                pump(&mut e, &mut c);
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            assert_eq!(c.decks[d].stems_source.as_deref(), Some(want), "deck {}", d + 1);
            c.route(&format!("Deck/{}/Eject", d + 1), &[OscType::Int(1)]);
            pump(&mut e, &mut c);
        }
        // The same audio under another name (moved, renamed, retagged): still prepared.
        let moved = path.with_file_name("reuse-renamed.wav");
        std::fs::copy(&path, &moved).unwrap();
        c.load_track(1, &moved);
        let deadline = Instant::now() + std::time::Duration::from_secs(60);
        while !(c.snap.decks[1].stems_ready && c.decks[1].stems_source.is_some()) {
            assert!(Instant::now() < deadline, "stems never came");
            pump(&mut e, &mut c);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(c.decks[1].stems_source.as_deref(), Some("prepared stems"));
    }

    #[test]
    fn a_cue_tapped_while_another_deck_plays_syncs_in() {
        let (mut e, mut c, _config) = core();
        for (d, name) in [(0, "sync-a.wav"), (1, "sync-b.wav")] {
            c.load_track(d, &kick_wav(name, 20));
            let deadline = Instant::now() + std::time::Duration::from_secs(60);
            while !c.snap.decks[d].loaded {
                assert!(Instant::now() < deadline, "track never loaded");
                pump(&mut e, &mut c);
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        for d in 0..2 {
            c.deck_cmd(d, DeckCmd::SetGrid(Some(dj_engine::track::BeatGrid { bpm: 120.0, anchor_secs: 0.0 })));
        }
        c.deck_cmd(0, DeckCmd::Play);
        c.deck_cmd(1, DeckCmd::Sync(false));
        c.deck_cmd(1, DeckCmd::SetCue { idx: 2, secs: Some(4.0), end_secs: None });
        for _ in 0..30 {
            pump(&mut e, &mut c);
        }
        assert!(c.snap.decks[0].playing && !c.snap.decks[1].sync);
        c.route("Deck/2/Cue3", &[OscType::Int(1)]);
        for _ in 0..30 {
            pump(&mut e, &mut c);
        }
        assert!(c.snap.decks[1].sync, "comes in synced");
        assert!(c.snap.decks[1].playing);
    }

    #[test]
    fn osc_loads_a_file_onto_a_deck() {
        let (mut e, mut c, _config) = core();
        let path = kick_wav("osc-load.wav", 5);
        assert!(c.route("/dj/Deck/3/Load".trim_start_matches("/dj/"), &[OscType::String(path.display().to_string())]));
        let deadline = Instant::now() + std::time::Duration::from_secs(60);
        while !c.snap.decks[2].loaded {
            assert!(Instant::now() < deadline, "never loaded");
            pump(&mut e, &mut c);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(c.decks[2].info.as_ref().unwrap().path, path);
    }

    #[test]
    fn mix_finishes_while_the_playing_deck_loops() {
        let (mut e, mut c, _config) = core();
        mix_setup(&mut e, &mut c);
        // The playing deck sits in a 1-bar loop: its own beat keeps jumping back.
        c.deck_cmd(0, DeckCmd::LoopBeats(4.0));
        for _ in 0..10 {
            pump(&mut e, &mut c);
        }
        assert!(c.snap.decks[0].loop_on);
        assert_eq!(c.mix_target(1), Some(0));
        let plan = c.plan_now(1, 0, crate::settings::MixStyle::Eq);
        c.start_mix(plan);
        run_mix(&mut e, &mut c, |_, _| {});
        assert!(!c.snap.decks[0].playing && c.snap.decks[1].playing, "mixed out of the loop");
        assert_eq!(c.session.channels[1].params.volume, 1.0);
    }

    #[test]
    fn mixes_start_on_phrases_and_bars_and_bring_the_new_track_all_in() {
        use crate::core::automix::{MixHow, MixWhen};
        let (mut e, mut c, _config) = core();
        mix_setup(&mut e, &mut c);
        // Clashing keys with stems on both decks: Stems, only drums and bass overlapping.
        c.decks[0].info.as_mut().unwrap().tags.key = dj_library::key::Key::parse("8A");
        c.decks[1].info.as_mut().unwrap().tags.key = dj_library::key::Key::parse("10A");
        assert!(c.keys_clash(1, 0));
        let (how, why) = c.assess(1, 0, MixWhen::Now);
        assert_eq!(how, MixHow::Stems, "{why}");
        assert!(why.contains("keys clash"));
        c.decks[1].info.as_mut().unwrap().tags.key = dj_library::key::Key::parse("8A");

        // The new track's cue is a beat into its bar (beat 17 at 120 BPM): it starts 3 beats early
        // so its bar lines fall on the playing track's.
        c.deck_cmd(1, DeckCmd::SetCue { idx: 0, secs: Some(8.5), end_secs: None });
        c.deck_cmd(1, DeckCmd::CueBeatLoop { idx: 0, beats: 4.0 });
        for _ in 0..60 {
            pump(&mut e, &mut c);
        }
        // Vocals throughout both tracks (the stand-in model's are too quiet to count): the old ones
        // play through the whole mix, so the new ones wait.
        for d in 0..2 {
            let bins = (c.snap.decks[d].duration_secs * dj_library::waveform::BINS_PER_SEC) as usize + 1;
            let w = crate::stems::StemWave { bins_per_sec: dj_library::waveform::BINS_PER_SEC, levels: std::array::from_fn(|_| vec![0.5; bins]) };
            c.decks[d].stem_wave = Some(Arc::new(w));
        }
        let plan = c.plan_now(1, 0, crate::settings::MixStyle::Stems);
        let beat = c.snap.decks[0].beat;
        // Never a quick mix (the old track isn't near its end): 20 s or more.
        assert!(!c.ends_within(0, 30.0));
        let secs = plan.length_beats * 60.0 / c.snap.decks[0].bpm;
        assert!(secs >= 20.0, "{secs:.1} s: {plan:?}");
        // Now: on a phrase (8 bars when that's within 4 bars, else 4 bars): at most 4 bars after
        // the new track's lead-in.
        assert_eq!(plan.start_beat % 16.0, 0.0, "{plan:?}");
        // (Ahead by the time it takes to start and its lead-in, then the next 4-bar line.)
        assert!(plan.start_beat - plan.lead_in > beat + 0.5 && plan.start_beat - beat <= 16.0 + 8.0, "{} {}", plan.start_beat, beat);
        // Coming in at the cue (a beat into its bar): 3 beats early; elsewhere it's on a bar line.
        let at_cue = plan.entry_secs.is_some_and(|t| (t - 8.5).abs() < 0.01);
        assert_eq!(plan.lead_in, if at_cue { 3.0 } else { 0.0 }, "{plan:?}");
        c.start_mix(plan);
        let mut checked = false;
        run_mix(&mut e, &mut c, |c, p| {
            if p > 0.2 && p < 0.9 {
                let d = (c.snap.decks[1].beat - c.snap.decks[0].beat).rem_euclid(4.0);
                assert!(d < 0.05 || d > 3.95, "bars line up: {d}");
                checked = true;
            }
        });
        assert!(checked);
        // The old vocals sang through the whole mix: the new ones took over before its end (the
        // new track all in before the old one's volume closed), not left out for later.
        assert_eq!(c.snap.decks[1].stem_gains, [1.0; 4]);
        assert!(c.vocal_return.is_none());
    }

    #[test]
    fn the_old_track_may_end_during_a_mix() {
        let (mut e, mut c, _config) = core();
        mix_setup(&mut e, &mut c);
        let plan = c.plan_now(1, 0, crate::settings::MixStyle::Eq);
        let start = plan.start_beat;
        c.start_mix(plan);
        // Once the mix is under way, the old track runs out (it fades on its own): no rush, the
        // mix goes on and finishes bringing the new one in.
        let mut blocks = 0;
        while c.mix_beat() < start + 4.0 {
            pump(&mut e, &mut c);
            blocks += 1;
            assert!(blocks < 48000 / 512 * 120, "mix never started");
        }
        assert!(c.automix.is_some());
        c.deck_cmd(0, DeckCmd::SeekFraction(0.999));
        run_mix(&mut e, &mut c, |_, _| {});
        assert!(!c.snap.decks[0].playing, "the old track ended");
        assert!(!c.status.iter().any(|(_, m)| m.contains("cancelled")), "{:?}", c.status);
        assert!(c.status.iter().any(|(_, m)| m.starts_with("Mixed into")), "{:?}", c.status);
        assert!(c.snap.decks[1].playing);
        assert!((c.session.channels[1].params.volume - 1.0).abs() < 1e-6);
        assert!((c.session.channels[1].params.eq[0] - 0.5).abs() < 1e-6, "the new bass in");
    }

    #[test]
    fn quitting_while_a_mix_echo_rings_doesnt_save_it() {
        let (mut e, mut c, _config) = core();
        mix_setup(&mut e, &mut c);
        let mut plan = c.plan_now(1, 0, crate::settings::MixStyle::Eq);
        plan.echo_at = Some(plan.length_beats / 4.0 - 1.0);
        c.start_mix(plan);
        run_mix(&mut e, &mut c, |_, _| {});
        let echo = |c: &Core| c.session.channels[0].fx.slots.iter().flatten().any(|s| s.name == "Echo");
        assert!(echo(&c) && !c.echo_cleanup.is_empty(), "its tail still ringing");
        c.prepare_to_quit();
        assert!(!echo(&c) && c.echo_cleanup.is_empty(), "gone before the session is saved");
    }

    #[test]
    fn mix_techniques_filter_and_echo() {
        let (mut e, mut c, _config) = core();
        mix_setup(&mut e, &mut c);
        let mut plan = c.plan_now(1, 0, crate::settings::MixStyle::Eq);
        let bars = plan.length_beats / 4.0;
        // Force every technique on this one.
        plan.filter = Some((0.0, bars));
        plan.echo_at = Some(bars - 1.0);
        c.start_mix(plan);
        let (mut swept, mut echoed) = (false, false);
        run_mix(&mut e, &mut c, |c, _| {
            if c.session.channels[0].params.cutoff > 0.6 {
                swept = true;
            }
            if c.session.channels[0].fx.slots.iter().flatten().any(|s| s.name == "Echo") {
                echoed = true;
            }
        });
        assert!(swept, "high-pass sweep on the old track");
        assert!(echoed, "echo inserted on the old track");
        assert_eq!(c.session.channels[0].params.cutoff, 0.5, "filter back");
        // The echo goes once its tail has rung out (2 bars).
        assert!(!c.echo_cleanup.is_empty());
        for _ in 0..200 {
            pump(&mut e, &mut c);
        }
        assert!(c.echo_cleanup.is_empty());
        assert!(!c.session.channels[0].fx.slots.iter().flatten().any(|s| s.name == "Echo"), "echo removed");
    }

    #[test]
    fn loaded_tracks_get_stems_in_the_background() {
        let (mut e, mut c, _config) = core();
        let dir = crate::settings::config_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("stems-test.wav");
        let spec = hound::WavSpec { channels: 2, sample_rate: 44100, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
        let mut w = hound::WavWriter::create(&path, spec).unwrap();
        for i in 0..44100 * 3 {
            let t = i as f32 / 44100.0;
            let v = 0.4 * (std::f32::consts::TAU * 60.0 * t).sin() + 0.2 * (std::f32::consts::TAU * 900.0 * t).sin();
            let s = (v * 30000.0) as i16;
            w.write_sample(s).unwrap();
            w.write_sample(s).unwrap();
        }
        w.finalize().unwrap();

        c.load_track(0, &path);
        let deadline = Instant::now() + std::time::Duration::from_secs(60);
        while !c.snap.decks[0].stems_ready {
            assert!(Instant::now() < deadline, "stems never attached");
            pump(&mut e, &mut c);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(c.decks[0].info.is_some());
        assert_eq!(c.decks[0].stems_progress, None);
        assert_eq!(c.decks[0].stems_source.as_deref(), Some("HT-Demucs"));
        // Kept as prepared stems for next time.
        assert!(std::fs::read_dir(c.settings.stems_prepared_dir()).unwrap().count() >= 1);
    }

    #[test]
    fn external_mixer_routes_each_deck_to_an_output_pair() {
        let (_e, mut c, _config) = core();
        // Xone:K2 style: 4 outputs = 2 stereo pairs. Left decks share pair 0, right decks pair 1.
        c.audio_channels = 4;
        c.set_external_mixer(true);
        assert!(c.external_routing());
        assert_eq!((0..4).map(|d| c.external_pair(d)).collect::<Vec<_>>(), vec![0, 1, 0, 1]);
        let p = c.engine_params(1, c.session.channels[1].params);
        assert_eq!(p.output, Output::Pair(1));
        let st = c.ext_stages();
        assert!(!st.trim && !st.eq && !st.filter && !st.volume, "no internal processing");
        // The session keeps the user's own routing.
        assert_eq!(c.session.channels[1].params.output, Output::Master);

        // 8 outputs: one pair per deck.
        c.audio_channels = 8;
        assert_eq!((0..4).map(|d| c.external_pair(d)).collect::<Vec<_>>(), vec![0, 1, 2, 3]);

        // A stereo-only device cannot do it: everything stays on the internal mixer.
        c.audio_channels = 2;
        assert!(!c.external_routing());
        assert_eq!(c.engine_params(1, c.session.channels[1].params).output, Output::Master);

        c.set_external_mixer(false);
        c.audio_channels = 4;
        assert!(!c.external_routing());
    }

    #[test]
    fn osc_root_and_mixer_routes() {
        let (mut e, mut c, _config) = core();
        assert_eq!(c.strip_root("/dj/Deck/1/PlayPause"), Some("Deck/1/PlayPause"));
        assert_eq!(c.strip_root("/DJ/mixer/ch1/volume"), Some("mixer/ch1/volume"));
        c.settings.osc_root = "/Syndicate/".into();
        assert_eq!(c.strip_root("/Syndicate/Mixer/CH3/Volume"), Some("Mixer/CH3/Volume"));
        assert_eq!(c.strip_root("/Other/Mixer/CH3/Volume"), None);
        assert!(c.route("Mixer/CH3/Volume", &[OscType::Float(0.25)]));
        assert!(c.route("Mixer/CH3/Trim", &[OscType::Float(1.0)]));
        assert!(c.route("Mixer/CH3/EQ/Low", &[OscType::Float(0.0)]));
        assert!(c.route("mixer/ch3/filter1", &[OscType::Float(0.2)]));
        let p = c.session.channels[2].params;
        assert_eq!(p.volume, 0.25);
        assert_eq!(p.trim_db, 6.0);
        assert_eq!(p.eq[0], 0.0);
        assert_eq!(p.cutoff, 0.2);
        assert!(c.route("Header/BPM", &[OscType::Float(131.0)]));
        pump(&mut e, &mut c);
        assert!((c.snap.clock.bpm - 131.0).abs() < 1e-6);
        assert!(!c.route("Nope/Thing", &[]));
        // Legacy native-FX routes from the original are accepted and ignored.
        assert!(c.route("Mixer/CH1/Reverb/Mix", &[OscType::Float(0.5)]));
    }

    #[test]
    fn midi_learn_and_toggle() {
        let (mut e, mut c, _config) = core();
        c.learn = true;
        c.touch("Mixer/CH2/Mute");
        c.handle_midi(&[0x91, 36, 127]);
        assert_eq!(c.profile.mappings.len(), 1);
        assert_eq!(c.profile.mappings[0].mode, MapMode::Toggle);
        c.learn = false;
        c.handle_midi(&[0x91, 36, 127]);
        c.handle_midi(&[0x81, 36, 0]);
        assert!(c.session.channels[1].params.mute);
        c.handle_midi(&[0x91, 36, 127]);
        assert!(!c.session.channels[1].params.mute);
        // CC on a knob: absolute.
        c.profile.mappings.push(MidiMapping {
            source: MidiSource { kind: MidiKind::Cc, channel: 0, number: 7 },
            address: "Mixer/CH1/Volume".into(),
            mode: MapMode::Absolute,
            invert: false,
            feedback: false,
        });
        c.handle_midi(&[0xb0, 7, 64]);
        assert!((c.session.channels[0].params.volume - 64.0 / 127.0).abs() < 1e-6);
        // ":value" suffix fixes the value sent.
        assert!(c.route("Deck/1/Quantize:6", &[]));
        assert_eq!(c.session.decks[0].quantize, dj_engine::deck::Quantize::Beats(0.5));
        pump(&mut e, &mut c);
    }

    #[test]
    fn fourteen_bit_faders_and_centred_jogs() {
        let (mut e, mut c, _config) = core();
        let map = |c: &mut Core, ch: u8, number: u8, address: &str, mode: MapMode| {
            c.profile.mappings.push(MidiMapping { source: MidiSource { kind: MidiKind::Cc, channel: ch, number }, address: address.into(), mode, invert: false, feedback: false });
        };
        // A 14-bit fader: coarse CC 19, fine CC 51 (Pioneer style).
        map(&mut c, 0, 19, "Mixer/CH1/Volume", MapMode::Absolute);
        c.handle_midi(&[0xb0, 19, 64]);
        assert!((c.session.channels[0].params.volume - 64.0 / 127.0).abs() < 1e-6, "coarse until a fine half is seen");
        c.handle_midi(&[0xb0, 51, 0]);
        assert!((c.session.channels[0].params.volume - 8192.0 / 16383.0).abs() < 1e-6, "{}", c.session.channels[0].params.volume);
        // From now on the coarse half waits for its fine half (no jump in between).
        c.handle_midi(&[0xb0, 19, 32]);
        assert!((c.session.channels[0].params.volume - 8192.0 / 16383.0).abs() < 1e-6);
        c.handle_midi(&[0xb0, 51, 100]);
        assert!((c.session.channels[0].params.volume - (32.0 * 128.0 + 100.0) / 16383.0).abs() < 1e-6);
        // CC 32–63 mapped on their own stay plain controls.
        map(&mut c, 0, 3, "Mixer/CH2/Volume", MapMode::Absolute);
        map(&mut c, 0, 35, "Mixer/CH3/Volume", MapMode::Absolute);
        c.handle_midi(&[0xb0, 35, 127]);
        assert_eq!(c.session.channels[2].params.volume, 1.0);

        // A jog wheel, centred on 64: paused, it moves through the track.
        let path = kick_wav("jog.wav", 30);
        c.load_track(0, &path);
        let deadline = Instant::now() + std::time::Duration::from_secs(60);
        while !(c.snap.decks[0].loaded && c.snap.decks[0].has_grid) {
            assert!(Instant::now() < deadline, "never loaded");
            pump(&mut e, &mut c);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        map(&mut c, 0, 33, "Deck/1/Jog", MapMode::RelativeCentred);
        let before = c.snap.decks[0].beat;
        for _ in 0..90 {
            c.handle_midi(&[0xb0, 33, 66]); // +2 ticks each
        }
        pump(&mut e, &mut c);
        let moved = c.snap.decks[0].beat - before;
        assert!((moved - 1.0).abs() < 0.05, "180 ticks: a beat forward ({moved})");
        for _ in 0..90 {
            c.handle_midi(&[0xb0, 33, 62]); // −2 ticks each
        }
        pump(&mut e, &mut c);
        assert!((c.snap.decks[0].beat - before).abs() < 0.05, "and back");
        // Playing: a nudge while it turns, released once it stops.
        c.deck_cmd(0, DeckCmd::Play);
        while !c.snap.decks[0].playing {
            assert!(Instant::now() < deadline, "never played");
            pump(&mut e, &mut c);
        }
        c.handle_midi(&[0xb0, 33, 65]);
        assert!(c.decks[0].jog_until.is_some());
        std::thread::sleep(std::time::Duration::from_millis(JOG_HOLD_MS + 20));
        c.tick();
        assert!(c.decks[0].jog_until.is_none());
    }

    #[test]
    fn the_opus_quad_preset_drives_dj() {
        let (mut e, mut c, _config) = core();
        let preset = crate::presets::opus_quad();
        // Every address is one dj knows, and no MIDI control is mapped twice.
        for m in &preset {
            // (A trigger with a press: dj ignores a trigger's 0.)
            let probe = match addr_kind(&m.address) {
                _ if m.address.contains(':') => vec![],
                Kind::Trigger => vec![OscType::Float(1.0)],
                Kind::Step => vec![OscType::Int(0)],
                _ => vec![OscType::Float(0.0)],
            };
            assert!(c.route(&m.address, &probe), "unknown address {}", m.address);
        }
        let mut sources: Vec<_> = preset.iter().map(|m| m.source).collect();
        sources.sort_by_key(|s| (s.kind as u8, s.channel, s.number));
        sources.dedup();
        assert_eq!(sources.len(), preset.len(), "a MIDI control mapped twice");
        // Added over an existing mapping on the same control: replaced; others kept.
        c.profile.mappings = vec![
            MidiMapping { source: MidiSource { kind: MidiKind::Note, channel: 0, number: 0 }, address: "Deck/1/Eject".into(), mode: MapMode::Absolute, invert: false, feedback: false },
            MidiMapping { source: MidiSource { kind: MidiKind::Note, channel: 9, number: 0 }, address: "Deck/1/Eject".into(), mode: MapMode::Absolute, invert: false, feedback: false },
        ];
        let n = crate::presets::apply(&mut c.profile, preset.clone());
        assert_eq!((n, c.profile.mappings.len()), (preset.len(), preset.len() + 1));

        let path = kick_wav("opus.wav", 30);
        c.load_track(1, &path);
        let deadline = Instant::now() + std::time::Duration::from_secs(60);
        while !(c.snap.decks[1].loaded && c.snap.decks[1].has_grid) {
            assert!(Instant::now() < deadline, "never loaded");
            pump(&mut e, &mut c);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        // Deck 2's PLAY/PAUSE (note 0, channel 2).
        c.handle_midi(&[0x91, 0, 127]);
        c.handle_midi(&[0x91, 0, 0]);
        while !c.snap.decks[1].playing {
            assert!(Instant::now() < deadline, "never played");
            pump(&mut e, &mut c);
        }
        // Its tempo fader, 14-bit: all the way to "+".
        c.handle_midi(&[0xb1, 0, 127]);
        c.handle_midi(&[0xb1, 32, 127]);
        pump(&mut e, &mut c);
        assert!((c.snap.decks[1].speed - 1.0 - c.snap.decks[1].tempo_range.speed(1.0) + 1.0).abs() < 1e-3, "{}", c.snap.decks[1].speed);
        // Mixer (channel 5): CH 2 fader, EQ low, headphone cue.
        c.handle_midi(&[0xb4, 18, 127]);
        c.handle_midi(&[0xb4, 9, 0]);
        c.handle_midi(&[0xb4, 71, 127]);
        c.handle_midi(&[0xb4, 71, 0]);
        let p = c.session.channels[1].params;
        assert_eq!((p.volume, p.eq[0]), (1.0, 0.0));
        assert!(p.cue, "headphone cue on");
        // The beat loop encoder doubles the loop size; the browse knob moves the selection.
        let size = c.loop_size[1];
        c.handle_midi(&[0xb1, 46, 1]);
        assert_eq!(c.loop_size[1], size * 2.0);
        c.handle_midi(&[0xb1, 46, 127]);
        assert_eq!(c.loop_size[1], size);
    }

    #[test]
    fn loading_onto_a_playing_deck_asks_first() {
        let (mut e, mut c, _config) = core();
        let (a, b) = (kick_wav("alert-a.wav", 30), kick_wav("alert-b.wav", 30));
        // A deck that isn't playing: straight in.
        c.request_load(0, &a);
        assert!(c.pending_load.is_none() && c.decks[0].loading.as_deref() == Some(a.as_path()));
        let deadline = Instant::now() + std::time::Duration::from_secs(60);
        while !(c.snap.decks[0].loaded && c.decks[0].info.is_some()) {
            assert!(Instant::now() < deadline, "never loaded");
            pump(&mut e, &mut c);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        c.deck_cmd(0, DeckCmd::Play);
        while !c.snap.decks[0].playing {
            assert!(Instant::now() < deadline, "never played");
            pump(&mut e, &mut c);
        }
        // Playing: it waits for an answer, the deck plays on.
        c.request_load(0, &b);
        assert_eq!(c.pending_load, Some((0, b.clone())));
        assert!(c.decks[0].loading.is_none());
        c.cancel_load();
        assert!(c.pending_load.is_none() && c.decks[0].loading.is_none());
        // A controller's LOAD (the browser's selection): asked; LOAD again for the same track loads.
        c.library = vec![dj_library::scan::Entry { path: b.clone(), ..Default::default() }];
        c.browser_selected = Some(0);
        assert!(c.route("Deck/1/LoadSelected", &[OscType::Float(1.0)]));
        assert_eq!(c.pending_load, Some((0, b.clone())));
        assert!(c.route("Deck/1/LoadSelected", &[OscType::Float(1.0)]));
        assert!(c.pending_load.is_none());
        assert_eq!(c.decks[0].loading.as_deref(), Some(b.as_path()), "confirmed");
        // dj's own loads (Auto-DJ, the session) don't ask.
        c.load_track(1, &a);
        assert!(c.pending_load.is_none());
    }

    #[test]
    fn loops_on_cues_switch_and_the_default_loop_is_4_beats() {
        let (mut e, mut c, _config) = core();
        assert_eq!(crate::settings::Settings::default().default_loop_beats, 4.0);
        assert_eq!(c.loop_size, [4.0; NUM_DECKS], "a new install starts at 4");
        c.load_track(0, &kick_wav("loops.wav", 30));
        let deadline = Instant::now() + std::time::Duration::from_secs(60);
        while !(c.snap.decks[0].loaded && c.snap.decks[0].has_grid) {
            assert!(Instant::now() < deadline, "never loaded");
            pump(&mut e, &mut c);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        c.deck_cmd(0, DeckCmd::SetCue { idx: 0, secs: Some(4.0), end_secs: None });
        pump(&mut e, &mut c);
        // On: a pad loops from its cue, of the loop length (8 here, chosen in Settings).
        c.settings.cue_pads = crate::settings::CuePadMode::Loop;
        c.set_default_loop(8.0);
        assert_eq!(c.loop_size, [8.0; NUM_DECKS]);
        assert!(matches!(c.pad_press(0, 0), Some(DeckCmd::CueBeatLoop { idx: 0, beats }) if beats == 8.0));
        // Off: it jumps there and plays on.
        c.settings.cue_pads = crate::settings::CuePadMode::Jump;
        assert!(matches!(c.pad_press(0, 0), Some(DeckCmd::HotCue { idx: 0, pressed: false })));
    }

    #[test]
    fn crates_are_made_shown_and_imported() {
        use dj_library::crates::Crate;
        use dj_library::scan::Entry;
        let (_e, mut c, _config) = core();
        let p = |n: &str| PathBuf::from(format!("/music/{n}.mp3"));
        c.library = ["a", "b", "c", "d"].iter().map(|n| Entry { path: p(n), title: n.to_string(), ..Default::default() }).collect();
        // Yours: names unique, sorted, shown when made.
        let warm = c.new_crate("Warm up");
        assert_eq!(c.browser_crate, Some(warm));
        c.add_to_crate(warm, &[p("c"), p("a"), p("c"), p("x")]);
        assert_eq!(c.crates[warm].tracks, vec![p("c"), p("a"), p("x")], "no doubles; order kept");
        // The browser lists it in its order (a track not in the library is left out), searched.
        assert_eq!(c.filtered_library(), vec![2, 0]);
        c.browser_query = "a".into();
        assert_eq!(c.filtered_library(), vec![0]);
        c.browser_query.clear();
        c.new_crate("Warm up");
        assert!(c.crates.iter().any(|k| k.name == "Warm up 2"));
        let a_crate = c.new_crate("A first");
        assert_eq!(a_crate, 0, "sorted by name");
        let warm = c.crates.iter().position(|k| k.name == "Warm up").unwrap();
        c.browser_crate = Some(warm);
        c.rename_crate(warm, "Opening");
        assert_eq!(c.crates[c.browser_crate.unwrap()].name, "Opening", "still shown after the re-sort");
        let shown = c.browser_crate.unwrap();
        c.remove_from_crate(shown, &p("c"));
        assert_eq!(c.filtered_library(), vec![0]);
        c.delete_crate(shown);
        assert_eq!(c.browser_crate, None);
        assert_eq!(c.filtered_library().len(), 4, "the whole library again");
        assert!(c.crates.iter().all(|k| k.name != "Opening"));
        // Imports: again from the same place replaces; a crate of yours by that name stays.
        let serato = |tracks: Vec<PathBuf>| vec![Crate { name: "A first".into(), tracks: tracks.clone(), source: "Serato".into() }, Crate { name: "House".into(), tracks, source: "Serato".into() }];
        c.merge_imported("Serato", serato(vec![p("a")]));
        c.merge_imported("Serato", serato(vec![p("b"), p("d")]));
        let house: Vec<_> = c.crates.iter().filter(|k| k.name == "House").collect();
        assert_eq!(house.len(), 1);
        assert_eq!(house[0].tracks, vec![p("b"), p("d")]);
        assert!(c.crates.iter().any(|k| k.name == "A first" && k.source.is_empty()), "yours kept");
        assert_eq!(c.crates.iter().filter(|k| k.name == "A first (Serato)").count(), 1);
        // Saved, and read back.
        crate::settings::flush_writes();
        let saved: Vec<Crate> = crate::settings::load_json(&Core::crates_path());
        assert_eq!(saved, c.crates);
    }

    #[test]
    fn address_kinds() {
        assert_eq!(addr_kind("Deck/1/PlayPause"), Kind::Trigger);
        assert_eq!(addr_kind("Deck/1/Cue3"), Kind::Trigger);
        assert_eq!(addr_kind("Deck/1/Cue"), Kind::Hold);
        assert_eq!(addr_kind("Deck/1/SlipLoop/Quarter"), Kind::Hold);
        assert_eq!(addr_kind("Deck/1/BeatSkip"), Kind::Step);
        assert_eq!(addr_kind("Mixer/CH1/Mute"), Kind::Switch);
        assert_eq!(addr_kind("Mixer/CH1/FX/Macro/2"), Kind::Value);
    }

    #[test]
    fn detected_cues_replace_close_ones_and_fill_free_pads() {
        let cue = |secs: f64, name: &str| Cue { kind: CueKind::Hot, secs, end_secs: None, name: name.into(), color: [0; 3] };
        let mut old: [Option<Cue>; NUM_HOT_CUES] = Default::default();
        old[0] = Some(cue(0.5, "Start"));
        old[2] = Some(cue(60.0, "Mine"));
        old[5] = Some(cue(120.0, "Outro"));
        // Bar = 2 s: Intro at 1.0 is close to pad 1; Drop at 30 is new; Outro at 121 is close to pad 6.
        let found = vec![cue(121.0, "Outro"), cue(30.0, "Drop"), cue(1.0, "Intro"), cue(31.0, "Drop again")];
        let (pads, replaced, added, dropped) = merge_cues(&old, found, 2.0);
        assert_eq!((replaced, added, dropped), (2, 1, 0));
        assert_eq!(pads[0].as_ref().unwrap().name, "Intro", "replaced on the same pad");
        assert_eq!(pads[5].as_ref().unwrap().secs, 121.0);
        assert_eq!(pads[2].as_ref().unwrap().name, "Mine", "nothing detected near it: kept");
        assert_eq!(pads[1].as_ref().unwrap().name, "Drop", "new ones on free pads");
        assert_eq!(pads.iter().flatten().count(), 4, "a second cue next to a new one isn't added");
        // Full pads: the rest are left out.
        let full: [Option<Cue>; NUM_HOT_CUES] = std::array::from_fn(|i| Some(cue(i as f64 * 10.0, "x")));
        let (_, r, a, d) = merge_cues(&full, vec![cue(5.0, "y"), cue(200.0, "z")], 2.0);
        assert_eq!((r, a, d), (0, 0, 2));
    }

    #[test]
    fn a_synced_decks_tempo_fader_sets_the_mix_tempo() {
        let (mut e, mut c, _config) = core();
        mix_setup(&mut e, &mut c);
        // Deck 2's track is at 124 BPM, synced to deck 1 (the master, 120 BPM).
        c.deck_cmd(1, DeckCmd::SetGrid(Some(dj_engine::track::BeatGrid { bpm: 124.0, anchor_secs: 0.0 })));
        c.deck_cmd(1, DeckCmd::Sync(true));
        c.deck_cmd(1, DeckCmd::Play);
        for _ in 0..40 {
            pump(&mut e, &mut c);
        }
        assert_eq!(c.snap.clock.owner, ClockOwner::Deck(0));
        assert!((c.snap.decks[1].speed - 120.0 / 124.0).abs() < 0.01, "following 120 BPM");
        // Reset (0 %) on the synced deck: its track at its own tempo, the master follows.
        assert!(c.route("Deck/2/TempoSlider", &[OscType::Float(0.0)]));
        for _ in 0..40 {
            pump(&mut e, &mut c);
        }
        assert!((c.snap.decks[1].speed - 1.0).abs() < 0.005, "deck 2 at 0 %: {}", c.snap.decks[1].speed);
        assert!((c.snap.clock.bpm - 124.0).abs() < 0.2, "the mix at 124: {}", c.snap.clock.bpm);
        // Its fader moves the mix: +2 % on deck 2 (a 10 % range: fader 0.2).
        c.route("Deck/2/TempoRange", &[OscType::Int(1)]);
        c.route("Deck/2/TempoSlider", &[OscType::Float(0.2)]);
        for _ in 0..40 {
            pump(&mut e, &mut c);
        }
        assert!((c.snap.decks[1].speed - 1.02).abs() < 0.005, "{}", c.snap.decks[1].speed);
        assert!((c.snap.clock.bpm - 124.0 * 1.02).abs() < 0.3, "{}", c.snap.clock.bpm);
    }

    #[test]
    fn volume_down_is_enough_for_mix() {
        let (mut e, mut c, _config) = core();
        mix_setup(&mut e, &mut c);
        // Deck 2 playing in your headphones (not looping), its fader down: Mix is offered.
        c.deck_cmd(1, DeckCmd::LoopExit);
        c.deck_cmd(1, DeckCmd::Play);
        for _ in 0..20 {
            pump(&mut e, &mut c);
        }
        assert!(c.snap.decks[1].playing && !c.snap.decks[1].loop_on);
        assert_eq!(c.mix_target(1), Some(0));
        // Fader up: you're mixing it yourself.
        c.route("Mixer/CH2/Volume", &[OscType::Float(0.6)]);
        assert_eq!(c.mix_target(1), None);
        c.route("Mixer/CH2/Volume", &[OscType::Float(0.0)]);
        // Mix: it waits in a silent loop where it is, then comes in where dj finds best.
        let plan = c.plan_best(1, 0, crate::settings::MixStyle::Auto);
        c.start_mix(plan);
        for _ in 0..5 {
            pump(&mut e, &mut c);
        }
        assert!(c.snap.decks[1].loop_on, "waiting in a loop");
        assert!(c.automix.is_some());
    }

    #[test]
    fn a_new_track_a_semitone_off_is_transposed_to_fit() {
        let (mut e, mut c, _config) = core();
        mix_setup(&mut e, &mut c);
        // Am (8A) playing; the new track in B flat minor (3A): a semitone apart, a clash.
        c.decks[0].info.as_mut().unwrap().tags.key = dj_library::key::Key::parse("8A");
        c.decks[1].info.as_mut().unwrap().tags.key = dj_library::key::Key::parse("3A");
        assert!(c.keys_clash(1, 0));
        assert_eq!(c.key_shift(1, 0), Some(-1));
        let plan = c.plan_best(1, 0, crate::settings::MixStyle::Auto);
        assert_eq!(plan.transpose, -1, "{plan:?}");
        assert!(plan.notes[0].contains("3A → 8A"), "{:?}", plan.notes);
        c.start_mix(plan);
        for _ in 0..5 {
            pump(&mut e, &mut c);
        }
        assert_eq!(c.snap.decks[1].semitones, -1.0);
        assert_eq!(c.deck_key(1).map(|k| k.camelot()).as_deref(), Some("8A"));
        assert!(!c.keys_clash(1, 0));
        // Cancelled: back to its own key.
        c.cancel_mix();
        pump(&mut e, &mut c);
        assert_eq!(c.snap.decks[1].semitones, 0.0);
        // Too far apart for a semitone (8A against 10A): no shift.
        c.decks[1].info.as_mut().unwrap().tags.key = dj_library::key::Key::parse("10A");
        assert_eq!(c.key_shift(1, 0), None);
    }
}
