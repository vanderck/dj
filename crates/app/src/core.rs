//! Application core: owns the session model, talks to the engine, and
//! routes every control surface (GUI, MIDI, keyboard, OSC) through one
//! address-based API that mirrors the original application's OSC API.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use collective_engine::deck::{DeckEvent, Quantize, TempoRange};
use collective_engine::engine::{ClockOwner, DeckCmd, DeckMode, SamplerCmd, NUM_DECKS};
use collective_engine::fx::{InsertPos, MacroTarget, MACRO_TARGETS, MAX_SLOTS, NUM_MACROS};
use collective_engine::mixer::{ChannelParams, MixMode, Output, XfAssign};
use collective_engine::sampler::{LaneParams, Step, LANES, STEPS};
use collective_engine::track::{Cue, CueKind, NUM_HOT_CUES};
use collective_engine::{Command, Controller, Event, Garbage, Snapshot};
use collective_library::scan::Entry;
use collective_library::{GridSource, Key, LoadedTrack, Tags, TrackOverrides, Waveform};
use crossbeam_channel::{unbounded, Receiver, Sender};
use rosc::OscType;

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
    /// Bar/beat last reported over OSC.
    last_beat: i64,
}

enum LoadJob {
    Track { deck: usize, path: PathBuf, overrides: Option<TrackOverrides>, replaygain: bool },
    Sample { deck: usize, lane: usize, path: PathBuf },
}

enum LoadResult {
    Track { deck: usize, path: PathBuf, result: Result<LoadedTrack, String> },
    Sample { deck: usize, lane: usize, path: PathBuf, result: Result<Arc<collective_engine::track::AudioData>, String> },
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
        | "pitchreset" | "phasereset" | "eject" | "loadselected" | "gridstarthere" | "master" | "resend" | "save"
        | "previous" | "next" | "tap" => Kind::Trigger,
        "playing" | "cuemomentary" | "slip" | "cueslip" | "faderstart" | "speedhalf" | "speeddouble" | "reverse"
        | "slipdotted" | "slipth" | "sliptriplet" | "pitchpreserve" | "mute" | "solo" | "fx" | "on" | "limiter"
        | "metronome" | "wallclock" | "sync" | "record" | "quantizeon" | "bypassall" => Kind::Switch,
        "cue" | "backspin" | "phaseminus" | "phaseplus" | "trigger" => Kind::Hold,
        "beatskip" | "barskip" | "loopmovebeat" | "loopmovebar" | "pitchstep" | "bpmstep" | "browse" => Kind::Step,
        "phasebend" | "bend" => Kind::Bend,
        "quantize" | "speed" | "output" | "lane" | "pattern" => Kind::Index,
        _ if SLIP_LOOPS.iter().any(|(n, _)| n.eq_ignore_ascii_case(last)) => Kind::Hold,
        _ => Kind::Value,
    }
}

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
    pub library_scanning: bool,
    library_rx: Option<Receiver<Vec<Entry>>>,
    pub browser_selected: Option<usize>,
    pub browser_query: String,
    loader_tx: Sender<LoadJob>,
    loader_rx: Receiver<LoadResult>,
    pub learn: bool,
    pub last_touched: Option<String>,
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
    dirty_since: Option<Instant>,
}

impl Core {
    pub fn new(ctl: Controller, sample_rate: f32, max_block: usize, settings: Settings, session: Session) -> Self {
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
                            LoadJob::Track { deck, path, overrides, replaygain } => LoadResult::Track {
                                deck,
                                result: collective_library::load(&path, overrides.as_ref(), replaygain).map_err(|e| e.to_string()),
                                path,
                            },
                            LoadJob::Sample { deck, lane, path } => LoadResult::Sample {
                                deck,
                                lane,
                                result: collective_library::load_sample(&path).map_err(|e| e.to_string()),
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
            library_scanning: false,
            library_rx: None,
            browser_selected: None,
            browser_query: String::new(),
            loader_tx: job_tx,
            loader_rx: res_rx,
            learn: false,
            last_touched: None,
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
            dirty_since: None,
        };
        // VST3 catalog from cache immediately; refresh in the background.
        let cache: ScanCache = crate::settings::load_json(&ScanCache::path());
        core.apply_scan(cache);
        core.library = crate::settings::load_json(&crate::settings::config_dir().join("library-cache.json"));
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

    /// Key used as the reference for harmonic hints: the master deck, else
    /// the first playing deck.
    pub fn reference_key(&self) -> Option<Key> {
        match self.snap.clock.owner {
            ClockOwner::Deck(d) => self.deck_key(d as usize),
            ClockOwner::Header => (0..NUM_DECKS).find(|&d| self.snap.decks[d].playing).and_then(|d| self.deck_key(d)),
        }
    }

    // ------------------------------------------------------------ startup / resync

    /// Push the whole session to a fresh engine (startup and after an audio
    /// device change).
    pub fn sync_engine(&mut self) {
        let s = self.settings.clone();
        self.send(Command::FaderCurve(s.fader_curve));
        self.send(Command::Crossovers { low_hz: s.crossover_low_hz, high_hz: s.crossover_high_hz });
        self.send(Command::ExtStages(s.ext_stages));
        self.send(Command::SplitCue(s.audio.split_cue));
        let ses = self.session.clone();
        self.send(Command::Wallclock(ses.wallclock));
        self.send(Command::SetBpm(ses.bpm));
        self.send(Command::GlobalQuantize(ses.global_quantize()));
        self.apply_master();
        self.apply_mix_mode();
        for ch in 0..NUM_DECKS {
            let c = &ses.channels[ch];
            self.send(Command::Channel { ch: ch as u8, params: c.params });
            self.send(Command::FaderStart { ch: ch as u8, on: c.fader_start });
            self.send(Command::XfAssign { ch: ch as u8, assign: c.xf_assign });
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
        self.send(Command::Metronome { on: m.metronome, gain: m.metronome_volume, to_master: m.metronome_to_master });
        self.mark_dirty();
    }

    pub fn apply_mix_mode(&mut self) {
        let s = &self.session;
        let mode = match s.mix_mode {
            MixMode::Crossfader { .. } => MixMode::Crossfader { pos: s.crossfader, sharp: s.crossfader_sharp },
            MixMode::Vector { .. } => MixMode::Vector { x: s.vector.0, y: s.vector.1 },
            m => m,
        };
        self.send(Command::MixMode(mode));
        self.mark_dirty();
    }

    // ------------------------------------------------------------ tracks

    pub fn load_track(&mut self, d: usize, path: &Path) {
        if d >= NUM_DECKS {
            return;
        }
        let overrides = self.trackdb.tracks.get(&TrackDb::key(path)).cloned();
        self.decks[d].loading = Some(path.to_path_buf());
        self.decks[d].error = None;
        let _ = self.loader_tx.send(LoadJob::Track {
            deck: d,
            path: path.to_path_buf(),
            overrides,
            replaygain: self.settings.use_replaygain,
        });
    }

    pub fn eject(&mut self, d: usize) {
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
        };
        self.send(Command::DeckMode { deck: d as u8, mode: DeckMode::Track });
        self.send(Command::Load { deck: d as u8, track: lt.track, start_at_first_cue: self.settings.load_at_first_cue });
        if self.settings.keylock_on_load {
            self.session.decks[d].keylock = true;
        }
        let kl = self.session.decks[d].keylock;
        self.deck_cmd(d, DeckCmd::KeyLock(kl));
        self.session.decks[d].mode = DeckMode::Track;
        self.session.decks[d].track = Some(lt.path.clone());
        *self.trackdb.play_counts.entry(TrackDb::key(&lt.path)).or_default() += 1;
        self.decks[d].info = Some(info);
        self.decks[d].loading = None;
        self.mark_dirty();
    }

    fn persist_cue(&mut self, d: usize, idx: u8, cue: Option<Cue>) {
        let Some(info) = self.decks[d].info.as_mut() else { return };
        if let Some(slot) = info.cues.get_mut(idx as usize) {
            *slot = cue;
        }
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
    pub fn set_grid(&mut self, d: usize, grid: collective_engine::track::BeatGrid) {
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
            self.set_grid(d, collective_engine::track::BeatGrid { bpm: v.track_bpm, anchor_secs: v.position_secs });
        }
    }

    pub fn nudge_grid(&mut self, d: usize, ms: f64) {
        let v = self.snap.decks[d];
        if v.has_grid {
            // anchor = position - beat * spb
            let spb = 60.0 / v.track_bpm;
            let anchor = v.position_secs - v.beat * spb + ms / 1000.0;
            self.set_grid(d, collective_engine::track::BeatGrid { bpm: v.track_bpm, anchor_secs: anchor });
        }
    }

    // ------------------------------------------------------------ library & scans

    pub fn rescan_library(&mut self) {
        if self.library_scanning {
            return;
        }
        let roots = self.settings.library_folders.clone();
        let cache = self.library.clone();
        let (tx, rx) = unbounded();
        std::thread::spawn(move || {
            let _ = tx.send(collective_library::scan::scan(&roots, &cache));
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

    pub fn filtered_library(&self) -> Vec<usize> {
        self.library.iter().enumerate().filter(|(_, e)| e.matches(&self.browser_query)).map(|(i, _)| i).collect()
    }

    // ------------------------------------------------------------ channel / mixer

    pub fn set_channel(&mut self, ch: usize, f: impl FnOnce(&mut ChannelParams)) {
        if ch >= NUM_DECKS {
            return;
        }
        let p = &mut self.session.channels[ch].params;
        f(p);
        let params = *p;
        self.send(Command::Channel { ch: ch as u8, params });
        self.mark_dirty();
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
                h.load_state(&collective_vst3::State { component: fxcat::unb64(c), controller: fxcat::unb64(e) });
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
        let dir = dirs::audio_dir().or_else(dirs::home_dir).unwrap_or_else(|| PathBuf::from(".")).join("Collective Recordings");
        let _ = std::fs::create_dir_all(&dir);
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let path = dir.join(format!("collective-{stamp}.wav"));
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

    /// Strip the configured root ("/Collective/", or "/Syndicate/" for
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
                self.route_deck(d, rest, f, on, trig, &ints)
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
            ["metronome"] => {
                if let Some(o) = on {
                    self.session.master.metronome = o;
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
            ["crossfader"] => {
                if let Some(v) = f {
                    self.session.crossfader = v.clamp(0.0, 1.0) * 2.0 - 1.0;
                    if !matches!(self.session.mix_mode, MixMode::Crossfader { .. }) {
                        self.session.mix_mode = MixMode::Crossfader { pos: 0.0, sharp: false };
                    }
                    self.apply_mix_mode();
                }
            }
            ["vectorx"] | ["vectory"] => {
                if let Some(v) = f {
                    if rest[0] == "vectorx" {
                        self.session.vector.0 = v.clamp(0.0, 1.0);
                    } else {
                        self.session.vector.1 = v.clamp(0.0, 1.0);
                    }
                    self.session.mix_mode = MixMode::Vector { x: 0.5, y: 0.5 };
                    self.apply_mix_mode();
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

    fn route_deck(&mut self, d: usize, rest: &[&str], f: Option<f32>, on: Option<bool>, trig: bool, ints: &[i32]) -> bool {
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
            ["cue"] => Some(DeckCmd::Cue(pressed)),
            ["gridstart"] if trig => Some(DeckCmd::GridStart),
            [c] if c.starts_with("cue") && c.len() == 4 => {
                let i = c[3..].parse::<u8>().ok().filter(|i| (1..=8).contains(i));
                if let (Some(i), true) = (i, trig) {
                    self.deck_cmd(d, DeckCmd::HotCue { idx: i - 1, pressed: true });
                    Some(DeckCmd::HotCue { idx: i - 1, pressed: false })
                } else {
                    None
                }
            }
            ["hotcue"] => match ints {
                [i] if (1..=8).contains(i) => {
                    self.deck_cmd(d, DeckCmd::HotCue { idx: (*i - 1) as u8, pressed: true });
                    Some(DeckCmd::HotCue { idx: (*i - 1) as u8, pressed: false })
                }
                [i, p, ..] if (1..=8).contains(i) => Some(DeckCmd::HotCue { idx: (*i - 1) as u8, pressed: *p != 0 }),
                _ => None,
            },
            ["setcueloop"] => ints.first().filter(|i| (1..=8).contains(*i)).map(|i| DeckCmd::CueLoop((*i - 1) as u8)),
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
            ["temposlider"] => f.map(|v| DeckCmd::Tempo(v.clamp(-1.0, 1.0) as f64)),
            ["tempo"] => f.map(|v| DeckCmd::Tempo((v.clamp(0.0, 1.0) * 2.0 - 1.0) as f64)),
            ["temporange"] => ints.first().map(|i| {
                let r = [TempoRange::Pct6, TempoRange::Pct10, TempoRange::Pct16, TempoRange::Pct50, TempoRange::Semitones12]
                    [(*i).clamp(0, 4) as usize];
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
            ["loopout"] if trig => Some(DeckCmd::LoopOut),
            ["loopexit"] if trig => Some(DeckCmd::LoopExit),
            ["loopbeats"] => f.filter(|v| *v > 0.0).map(|v| DeckCmd::LoopBeats(v as f64)),
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
            ["pitchdown"] if trig => Some(DeckCmd::Semitones(self.snap.decks[d].semitones - 1.0)),
            ["pitchup"] if trig => Some(DeckCmd::Semitones(self.snap.decks[d].semitones + 1.0)),
            ["pitchreset"] if trig => Some(DeckCmd::Semitones(0.0)),
            ["pitchstep"] => ints.first().map(|n| DeckCmd::Semitones(self.snap.decks[d].semitones + *n as f64)),
            ["pitch"] => f.map(|v| DeckCmd::Semitones((v as f64).round())),
            ["phaseminus"] => Some(DeckCmd::PhaseBend(if pressed { -1.0 } else { 0.0 })),
            ["phaseplus"] => Some(DeckCmd::PhaseBend(if pressed { 1.0 } else { 0.0 })),
            ["phasebend"] => f.map(|v| DeckCmd::PhaseBend(v.signum() as f64 * (v != 0.0) as i32 as f64)),
            ["phasereset"] if trig => Some(DeckCmd::PhaseReset),
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
            ["loadselected"] if trig => {
                if let Some(e) = self.browser_selected.and_then(|i| self.library.get(i)).map(|e| e.path.clone()) {
                    self.load_track(d, &e);
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
        use collective_engine::sampler::{Direction, Rhythm};
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
            ["xfader"] => {
                if let Some(&i) = ints.first() {
                    let a = [XfAssign::A, XfAssign::Thru, XfAssign::B][i.clamp(0, 2) as usize];
                    self.session.channels[c].xf_assign = a;
                    self.send(Command::XfAssign { ch: c as u8, assign: a });
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
            ["header", "metronome"] => self.session.master.metronome,
            ["header", "wallclock"] => self.session.wallclock,
            ["header", "quantizeon"] => self.session.quantize_on,
            ["header", "master"] => self.snap.clock.owner == ClockOwner::Header,
            ["header", "record"] => self.recorder.is_some(),
            _ => return None,
        })
    }

    // ------------------------------------------------------------ MIDI input

    fn handle_midi(&mut self, msg: &[u8]) {
        if msg.is_empty() {
            return;
        }
        let status = msg[0] & 0xf0;
        let ch = msg[0] & 0x0f;
        let (src, value) = match (status, msg.get(1), msg.get(2)) {
            (0x90, Some(&n), Some(&v)) => (MidiSource { kind: MidiKind::Note, channel: ch, number: n }, v as f32 / 127.0),
            (0x80, Some(&n), _) => (MidiSource { kind: MidiKind::Note, channel: ch, number: n }, 0.0),
            (0xb0, Some(&n), Some(&v)) => (MidiSource { kind: MidiKind::Cc, channel: ch, number: n }, v as f32 / 127.0),
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
            let kind = addr_kind(&m.address);
            let v = if m.invert { 1.0 - value } else { value };
            match (m.mode, kind) {
                (MapMode::Toggle, _) => {
                    if v > 0.5 {
                        let cur = self.switch_state(&m.address).unwrap_or(false);
                        self.route(&m.address, &[OscType::Int(!cur as i32)]);
                    }
                }
                (MapMode::Relative, _) => {
                    let step = if raw < 64 { raw as i32 } else { raw as i32 - 128 };
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
                    let scaled = if lower.ends_with("temposlider") || lower.ends_with("phasebend") {
                        v * 2.0 - 1.0
                    } else if lower.ends_with("/pitch") {
                        (v * 48.0 - 24.0).round()
                    } else if lower.ends_with("header/bpm") {
                        60.0 + v * 140.0
                    } else {
                        v
                    };
                    self.route(&m.address, &[OscType::Float(scaled)]);
                }
            }
        }
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
        out.push(("Header/Metronome".into(), vec![OscType::Int(self.session.master.metronome as i32)]));
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
            if let Ok(v) = rx.try_recv() {
                self.library = v;
                self.library_scanning = false;
                self.library_rx = None;
                let _ = crate::settings::save_json(&crate::settings::config_dir().join("library-cache.json"), &self.library);
                self.log(format!("Library: {} tracks", self.library.len()));
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

        // VST3 latency changes and in-plugin edits.
        for ch in 0..NUM_DECKS {
            for slot in 0..MAX_SLOTS {
                if let Some(FxLive::Vst3(h)) = self.fx_live[ch][slot].clone() {
                    let flags = h.take_restart_flags();
                    if collective_vst3::Vst3Handle::latency_changed(flags) {
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

        self.feedback_state();
        self.midi_feedback();

        if let Some(t) = self.dirty_since {
            if t.elapsed() > Duration::from_secs(3) {
                self.save_all();
            }
        }
    }

    pub fn save_all(&mut self) {
        self.dirty_since = None;
        self.snapshot_plugin_states();
        let _ = self.session.save();
        let _ = self.settings.save();
        let _ = self.trackdb.save();
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

#[cfg(test)]
mod tests {
    use super::*;
    use collective_engine::{create, EngineConfig};

    fn core() -> (collective_engine::Engine, Core) {
        let dir = std::env::temp_dir().join(format!("collective-core-test-{}-{:?}", std::process::id(), std::thread::current().id()));
        std::env::set_var("COLLECTIVE_CONFIG_DIR", &dir);
        let (engine, ctl) = create(EngineConfig { sample_rate: 48000.0, max_block: 512, max_latency: 4096 });
        let settings = Settings { osc_enabled: false, vst3_enabled: false, library_folders: vec![], ..Default::default() };
        let mut c = Core::new(ctl, 48000.0, 512, settings, Session::default());
        c.sync_engine();
        (engine, c)
    }

    fn pump(e: &mut collective_engine::Engine, c: &mut Core) {
        for _ in 0..4 {
            e.process(512);
        }
        c.tick();
    }

    #[test]
    fn fx_chain_macros_and_presets() {
        let (mut e, mut c) = core();
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
    fn osc_root_and_mixer_routes() {
        let (mut e, mut c) = core();
        assert_eq!(c.strip_root("/Collective/Deck/1/PlayPause"), Some("Deck/1/PlayPause"));
        assert_eq!(c.strip_root("/collective/mixer/ch1/volume"), Some("mixer/ch1/volume"));
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
        let (mut e, mut c) = core();
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
        assert_eq!(c.session.decks[0].quantize, collective_engine::deck::Quantize::Beats(0.5));
        pump(&mut e, &mut c);
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
}
