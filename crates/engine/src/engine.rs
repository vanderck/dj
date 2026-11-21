//! The realtime engine: owns decks, samplers, mixer and clock, and talks to
//! the rest of the app exclusively through lock-free ring buffers.
//!
//! * [`Command`]s flow UI → audio.
//! * [`Event`]s (including objects to be dropped off the audio thread) and
//!   [`Snapshot`]s flow audio → UI.
//!
//! Nothing in [`Engine::process`] allocates or locks.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use rtrb::{Consumer, Producer, RingBuffer};
use serde::{Deserialize, Serialize};

use crate::deck::{Deck, DeckCtx, DeckEvent, DeckView, Quantize, TempoRange};
use crate::dsp::{db_to_gain, Frame, Limiter, Meter};
use crate::fx::{Effect, FxCtx, FxSlot, InsertPos, MacroTarget, SlotHealth, MACRO_TARGETS, MAX_SLOTS};
use crate::mixer::{mix_gains, Channel, ChannelParams, ExtStages, FaderCurve, MixMode, Output, XfAssign};
use crate::sampler::{LaneParams, Sampler, SamplerView, Step, LANES};
use crate::track::{AudioData, BeatGrid, StemData, Track};

pub const NUM_DECKS: usize = 4;
pub const MAX_PAIRS: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum DeckMode {
    #[default]
    Track,
    Sampler,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ClockOwner {
    #[default]
    Header,
    Deck(u8),
}

#[derive(Clone, Copy, Debug)]
pub enum DeckCmd {
    Play,
    Pause,
    TogglePlay,
    SetPlaying(bool),
    Cue(bool),
    GridStart,
    HotCue { idx: u8, pressed: bool },
    CueLoop(u8),
    /// Launch a hot cue with a loop of this many beats starting at it.
    CueBeatLoop { idx: u8, beats: f64 },
    DeleteCue(u8),
    SetCue { idx: u8, secs: Option<f64>, end_secs: Option<f64> },
    MomentaryCues(bool),
    LoopToggle,
    LoopBeats(f64),
    DefaultLoopBeats(f64),
    LoopIn,
    LoopOut,
    LoopExit,
    /// On the next beat: leave the loop, jumping to its start (or to hot cue `to_cue`, or to track
    /// time `to_secs`).
    ReleaseLoop { to_cue: Option<u8>, to_secs: Option<f64> },
    LoopScale(f64),
    FreeLoop(f64),
    LoopMove(f64),
    SlipLoop { beats: f64, pressed: bool },
    Skip(f64),
    SeekFraction(f64),
    Tempo(f64),
    TempoRange(TempoRange),
    SpeedMult(f64),
    Sync(bool),
    ToggleSync,
    ToggleMaster,
    KeyLock(bool),
    Semitones(f64),
    PhaseBend(f64),
    PhaseReset,
    Reverse(bool),
    Backspin(bool),
    Slip(bool),
    Quantize(Quantize),
    SetGrid(Option<BeatGrid>),
    GridStartHere,
    /// Tempo bend on the master deck (BPM, held), 0 releases.
    BpmBend(f64),
    TapeStopMs(f64),
    /// Per-stem gain (0 = muted, 1 = unity) in [`crate::track::Stem`] order. Takes effect once
    /// stems are attached; resets to unity when a new track loads.
    StemGain { stem: u8, gain: f32 },
}

#[derive(Clone, Copy, Debug)]
pub enum SamplerCmd {
    Toggle,
    Play,
    Stop,
    Trigger { lane: u8, velocity: f32 },
    Lane { lane: u8, params: LaneParams },
    Step { pattern: u8, lane: u8, step: u8, value: Step },
    Pattern(u8),
    Swing(f32),
    GlobalPitch(f32),
    Speed(f64),
}

pub enum Command {
    Load { deck: u8, track: Arc<Track>, start_at_first_cue: bool },
    Eject { deck: u8 },
    /// Separated stems for the track loaded on `deck` (ignored if that track was replaced).
    AttachStems { deck: u8, stems: Arc<StemData> },
    Deck { deck: u8, cmd: DeckCmd },
    DeckMode { deck: u8, mode: DeckMode },
    Sampler { deck: u8, cmd: SamplerCmd },
    SamplerSample { deck: u8, lane: u8, audio: Option<Arc<AudioData>> },

    Channel { ch: u8, params: ChannelParams },
    FaderStart { ch: u8, on: bool },
    XfAssign { ch: u8, assign: XfAssign },
    MixMode(MixMode),
    FaderCurve(FaderCurve),
    Crossovers { low_hz: f32, high_hz: f32 },
    ExtStages(ExtStages),

    FxInsert { ch: u8, slot: u8, effect: Box<dyn Effect>, pos: InsertPos, on: bool, mix: f32 },
    FxRemove { ch: u8, slot: u8 },
    FxMove { ch: u8, from: u8, to: u8 },
    FxSlot { ch: u8, slot: u8, pos: InsertPos, on: bool, mix: f32 },
    FxParam { ch: u8, slot: u8, param: u32, value: f32 },
    FxBypassAll { ch: u8, on: bool },
    FxMacro { ch: u8, idx: u8, value: f32 },
    FxMacroTargets { ch: u8, idx: u8, targets: [Option<MacroTarget>; MACRO_TARGETS] },
    /// An effect reported a new latency (e.g. a VST3 restartComponent).
    FxLatency { ch: u8, slot: u8, samples: u32 },
    /// Re-arm a slot that was flagged as failed.
    FxRearm { ch: u8, slot: u8 },

    MasterGain(f32),
    Limiter(bool),
    CueGain(f32),
    /// 0 = cue only, 1 = master only.
    CueMix(f32),
    SplitCue(bool),

    SetBpm(f64),
    HeaderBend(f64),
    HeaderMaster,
    Wallclock(bool),
    GlobalQuantize(Option<f64>),
    Metronome { on: bool, gain: f32, to_master: bool },

    StartRecording(Producer<Frame>),
    StopRecording,
}

/// Objects released by the audio thread, to be dropped elsewhere.
pub enum Garbage {
    Track(Arc<Track>),
    Stems(Arc<StemData>),
    Audio(Arc<AudioData>),
    Fx(FxSlot),
    Recorder(Producer<Frame>),
    Command(Command),
}

pub enum Event {
    Garbage(Garbage),
    Deck { deck: u8, event: DeckEvent },
    /// Fired on every master beat. `bar` is 1-based, `beat` 1..=4.
    Beat { bar: i64, beat: u8 },
    FxFailed { ch: u8, slot: u8 },
    ClockOwner(ClockOwner),
    RecordingOverflow,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ChannelView {
    pub pre_peak: [f32; 2],
    pub post_peak: [f32; 2],
    pub post_rms: [f32; 2],
    pub fx: [Option<(bool, SlotHealth, f32)>; MAX_SLOTS],
    pub fx_latency: (u32, u32),
    pub mix_gain: f32,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ClockView {
    pub bpm: f64,
    pub beat: f64,
    pub owner: ClockOwner,
    pub wallclock: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Snapshot {
    pub decks: [DeckView; NUM_DECKS],
    pub deck_mode: [DeckMode; NUM_DECKS],
    pub samplers: [SamplerView; NUM_DECKS],
    pub channels: [ChannelView; NUM_DECKS],
    pub master: Meter,
    pub cue: Meter,
    pub limiter_db: f32,
    pub clock: ClockView,
    pub global_quantize: Option<f64>,
    pub cpu_load: f32,
    pub compensation_latency: u32,
    pub recording: bool,
    pub frames: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct EngineConfig {
    pub sample_rate: f32,
    pub max_block: usize,
    /// Upper bound for plugin delay compensation, in samples.
    pub max_latency: usize,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self { sample_rate: 48000.0, max_block: 1024, max_latency: 16384 }
    }
}

/// UI-side handle.
pub struct Controller {
    commands: Producer<Command>,
    events: Consumer<Event>,
    snapshots: Arc<Mutex<Snapshot>>,
    last: Snapshot,
}

impl Controller {
    /// Send a command. Returns it back if the queue is full.
    pub fn send(&mut self, cmd: Command) -> Result<(), Command> {
        self.commands.push(cmd).map_err(|rtrb::PushError::Full(c)| c)
    }

    pub fn poll_event(&mut self) -> Option<Event> {
        self.events.pop().ok()
    }

    /// Latest snapshot published by the audio thread.
    pub fn snapshot(&mut self) -> &Snapshot {
        if let Ok(s) = self.snapshots.lock() {
            self.last = *s;
        }
        &self.last
    }
}

struct Metronome {
    on: bool,
    gain: f32,
    to_master: bool,
    phase: f32,
    env: f32,
    freq: f32,
}

pub struct Engine {
    cfg: EngineConfig,
    commands: Consumer<Command>,
    events: Producer<Event>,
    /// Latest-value slot. The audio thread only ever `try_lock`s it, so it
    /// never blocks; the UI holds the lock just long enough to copy.
    snapshots: Arc<Mutex<Snapshot>>,

    decks: [Deck; NUM_DECKS],
    samplers: [Sampler; NUM_DECKS],
    mode: [DeckMode; NUM_DECKS],
    channels: [Channel; NUM_DECKS],
    fader_start: [bool; NUM_DECKS],
    xf_assign: [XfAssign; NUM_DECKS],
    mix_mode: MixMode,
    curve: FaderCurve,
    tape_stop_ms: [f64; NUM_DECKS],
    sync_before_master: [bool; NUM_DECKS],

    header_bpm: f64,
    header_bend: f64,
    clock_beat: f64,
    owner: ClockOwner,
    wallclock: bool,
    global_quantize: Option<f64>,

    master_gain: f32,
    cue_gain: f32,
    cue_mix: f32,
    split_cue: bool,
    limiter: Limiter,
    master_meter: Meter,
    cue_meter: Meter,
    metronome: Metronome,
    compensation: usize,

    deck_buf: Vec<Frame>,
    master_buf: Vec<Frame>,
    cue_buf: Vec<Frame>,
    pair_bufs: Vec<Vec<Frame>>,
    pair_used: [bool; MAX_PAIRS],

    recorder: Option<Producer<Frame>>,
    frames: u64,
    load: f32,
    snapshot_every: u64,
    since_snapshot: u64,
}

pub fn create(cfg: EngineConfig) -> (Engine, Controller) {
    let (cp, cc) = RingBuffer::new(4096);
    let (ep, ec) = RingBuffer::new(4096);
    let shared = Arc::new(Mutex::new(Snapshot::default()));
    let sr = cfg.sample_rate;
    let engine = Engine {
        cfg,
        commands: cc,
        events: ep,
        snapshots: shared.clone(),
        decks: std::array::from_fn(|_| Deck::new(sr as f64)),
        samplers: std::array::from_fn(|_| Sampler::new(sr as f64)),
        mode: [DeckMode::Track; NUM_DECKS],
        channels: std::array::from_fn(|_| Channel::new(sr, cfg.max_block, cfg.max_latency)),
        fader_start: [false; NUM_DECKS],
        xf_assign: [XfAssign::A, XfAssign::B, XfAssign::Thru, XfAssign::Thru],
        mix_mode: MixMode::Off,
        curve: FaderCurve::LinearDb,
        tape_stop_ms: [25.0; NUM_DECKS],
        sync_before_master: [true; NUM_DECKS],
        header_bpm: 128.0,
        header_bend: 0.0,
        clock_beat: 0.0,
        owner: ClockOwner::Header,
        wallclock: false,
        global_quantize: Some(4.0),
        master_gain: 1.0,
        cue_gain: 1.0,
        cue_mix: 0.0,
        split_cue: false,
        limiter: Limiter::new(sr),
        master_meter: Meter::default(),
        cue_meter: Meter::default(),
        metronome: Metronome { on: false, gain: 0.5, to_master: false, phase: 0.0, env: 0.0, freq: 1000.0 },
        compensation: 0,
        deck_buf: vec![[0.0; 2]; cfg.max_block],
        master_buf: vec![[0.0; 2]; cfg.max_block],
        cue_buf: vec![[0.0; 2]; cfg.max_block],
        pair_bufs: (0..MAX_PAIRS).map(|_| vec![[0.0; 2]; cfg.max_block]).collect(),
        pair_used: [false; MAX_PAIRS],
        recorder: None,
        frames: 0,
        load: 0.0,
        snapshot_every: (sr as u64 / 60).max(1),
        since_snapshot: u64::MAX / 2,
    };
    let controller = Controller { commands: cp, events: ec, snapshots: shared, last: Snapshot::default() };
    (engine, controller)
}

impl Engine {
    pub fn sample_rate(&self) -> f32 {
        self.cfg.sample_rate
    }

    pub fn max_block(&self) -> usize {
        self.cfg.max_block
    }

    /// Total master-path latency: plugin delay compensation + limiter look-ahead.
    pub fn output_latency(&self) -> usize {
        self.compensation + self.limiter.latency()
    }

    pub fn master(&self, n: usize) -> &[Frame] {
        &self.master_buf[..n]
    }

    pub fn cue(&self, n: usize) -> &[Frame] {
        &self.cue_buf[..n]
    }

    /// Direct-output pair buffer, or `None` if no channel used it this block.
    pub fn pair(&self, p: usize, n: usize) -> Option<&[Frame]> {
        if p < MAX_PAIRS && self.pair_used[p] {
            Some(&self.pair_bufs[p][..n])
        } else {
            None
        }
    }

    fn emit(&mut self, e: Event) {
        if let Err(rtrb::PushError::Full(e)) = self.events.push(e) {
            // Queue full: we have to drop here (rare; only under UI stalls).
            drop(e);
        }
    }

    fn trash(&mut self, g: Garbage) {
        self.emit(Event::Garbage(g));
    }

    fn deck_ctx(&self, d: usize) -> DeckCtx {
        DeckCtx {
            out_rate: self.cfg.sample_rate as f64,
            clock_bpm: self.clock_bpm(),
            clock_beat: self.clock_beat,
            global_quantize: self.global_quantize,
            is_clock_owner: self.owner == ClockOwner::Deck(d as u8),
            tape_stop_ms: self.tape_stop_ms[d],
        }
    }

    fn clock_bpm(&self) -> f64 {
        match self.owner {
            ClockOwner::Header => (self.header_bpm + self.header_bend).clamp(20.0, 999.0),
            ClockOwner::Deck(d) => {
                self.decks[d as usize].effective_bpm().unwrap_or(self.header_bpm).clamp(20.0, 999.0)
            }
        }
    }

    fn set_owner(&mut self, o: ClockOwner) {
        if self.owner == o {
            return;
        }
        // Hand over at the current tempo so nothing jumps.
        let bpm = self.clock_bpm();
        if let ClockOwner::Deck(prev) = self.owner {
            let p = prev as usize;
            self.decks[p].set_sync(self.sync_before_master[p]);
        }
        if let ClockOwner::Deck(d) = o {
            let d = d as usize;
            self.sync_before_master[d] = self.decks[d].sync() || self.sync_before_master[d];
            self.decks[d].set_sync(false);
            if self.decks[d].is_playing() {
                self.decks[d].set_bpm(bpm);
            }
        }
        self.header_bpm = bpm;
        self.owner = o;
        self.emit(Event::ClockOwner(o));
    }

    /// Automatic handoff rules: a master deck that stops or is ejected passes
    /// the clock to the lowest-numbered playing synced deck, else the header.
    fn maintain_owner(&mut self) {
        if self.wallclock {
            if self.owner != ClockOwner::Header {
                self.set_owner(ClockOwner::Header);
            }
            return;
        }
        if self.owner == ClockOwner::Header {
            // dj picks the master: the first playing deck with a grid (synced ones first) takes
            // the clock at its own tempo, so nothing follows a tempo nobody set.
            let pick = (0..NUM_DECKS)
                .filter(|&i| self.mode[i] == DeckMode::Track && self.decks[i].is_playing() && self.decks[i].grid().is_some())
                .min_by_key(|&i| !self.decks[i].sync());
            if let Some(i) = pick {
                if let Some(b) = self.decks[i].effective_bpm() {
                    self.header_bpm = b - self.header_bend;
                }
                self.set_owner(ClockOwner::Deck(i as u8));
            }
            return;
        }
        if let ClockOwner::Deck(d) = self.owner {
            let deck = &self.decks[d as usize];
            let ok = deck.is_loaded() && deck.grid().is_some() && self.mode[d as usize] == DeckMode::Track;
            if !ok || !deck.is_playing() {
                let next = (0..NUM_DECKS).find(|&i| {
                    i != d as usize
                        && self.mode[i] == DeckMode::Track
                        && self.decks[i].is_playing()
                        && self.decks[i].sync()
                        && self.decks[i].grid().is_some()
                });
                if !ok || next.is_some() {
                    match next {
                        Some(i) => self.set_owner(ClockOwner::Deck(i as u8)),
                        None => self.set_owner(ClockOwner::Header),
                    }
                }
            }
        }
    }

    fn update_latency(&mut self) {
        let lats: [(usize, usize); NUM_DECKS] = std::array::from_fn(|i| self.channels[i].fx.latency());
        let max = lats.iter().map(|(a, b)| a + b).max().unwrap_or(0).min(self.cfg.max_latency);
        self.compensation = max;
        for (i, ch) in self.channels.iter_mut().enumerate() {
            let (pre, post) = lats[i];
            ch.comp_delay.set_delay(max.saturating_sub(pre + post));
            ch.cue_delay.set_delay(max.saturating_sub(pre));
        }
    }

    fn handle_deck(&mut self, d: usize, cmd: DeckCmd) {
        let ctx = self.deck_ctx(d);
        if self.mode[d] == DeckMode::Sampler {
            // Transport routes are shared with the sampler where it makes sense.
            match cmd {
                DeckCmd::Play | DeckCmd::SetPlaying(true) => self.samplers[d].play(self.clock_beat, ctx.global_quantize),
                DeckCmd::Pause | DeckCmd::SetPlaying(false) => self.samplers[d].stop(),
                DeckCmd::TogglePlay => self.samplers[d].toggle(self.clock_beat, ctx.global_quantize),
                DeckCmd::SpeedMult(m) => self.samplers[d].speed = m,
                _ => {}
            }
            return;
        }
        let deck = &mut self.decks[d];
        match cmd {
            DeckCmd::Play => deck.play(&ctx),
            DeckCmd::Pause => deck.pause(),
            DeckCmd::TogglePlay => deck.toggle_play(&ctx),
            DeckCmd::SetPlaying(on) => deck.set_playing(on, &ctx),
            DeckCmd::Cue(p) => deck.cue(p, &ctx),
            DeckCmd::GridStart => deck.jump_to_grid_start(),
            DeckCmd::HotCue { idx, pressed } => deck.hot_cue(idx, pressed, &ctx),
            DeckCmd::CueLoop(i) => deck.cue_loop(i, &ctx),
            DeckCmd::CueBeatLoop { idx, beats } => deck.cue_beat_loop(idx, beats, &ctx),
            DeckCmd::DeleteCue(i) => deck.delete_hot_cue(i),
            DeckCmd::SetCue { idx, secs, end_secs } => {
                let cue = secs.map(|s| crate::track::Cue {
                    kind: if end_secs.is_some() { crate::track::CueKind::Loop } else { crate::track::CueKind::Hot },
                    secs: s,
                    end_secs,
                    name: String::new(),
                    color: [0; 3],
                });
                deck.set_hot_cue(idx, cue.as_ref());
            }
            DeckCmd::MomentaryCues(on) => deck.set_momentary_cues(on),
            DeckCmd::LoopToggle => deck.loop_toggle(&ctx),
            DeckCmd::LoopBeats(b) => deck.loop_beats(b, &ctx),
            DeckCmd::DefaultLoopBeats(b) => deck.default_loop_beats = b,
            DeckCmd::LoopIn => deck.loop_in(&ctx),
            DeckCmd::LoopOut => deck.loop_out(&ctx),
            DeckCmd::LoopExit => deck.loop_exit(),
            DeckCmd::ReleaseLoop { to_cue, to_secs } => deck.release_loop(to_cue, to_secs, &ctx),
            DeckCmd::LoopScale(f) => deck.loop_scale(f),
            DeckCmd::FreeLoop(ms) => deck.free_loop(ms),
            DeckCmd::LoopMove(b) => deck.loop_move(b),
            DeckCmd::SlipLoop { beats, pressed } => deck.slip_loop(beats, pressed, &ctx),
            DeckCmd::Skip(b) => deck.skip_beats(b),
            DeckCmd::SeekFraction(f) => deck.seek_fraction(f, &ctx),
            DeckCmd::Tempo(v) => deck.set_tempo_fader(v),
            DeckCmd::TempoRange(r) => deck.set_tempo_range(r),
            DeckCmd::SpeedMult(m) => deck.set_speed_mult(m),
            DeckCmd::Sync(on) => deck.set_sync(on),
            DeckCmd::ToggleSync => {
                let s = !deck.sync();
                deck.set_sync(s)
            }
            DeckCmd::ToggleMaster => {
                if !self.wallclock {
                    if self.owner == ClockOwner::Deck(d as u8) {
                        self.set_owner(ClockOwner::Header);
                    } else if self.decks[d].grid().is_some() {
                        self.set_owner(ClockOwner::Deck(d as u8));
                    }
                }
            }
            DeckCmd::KeyLock(on) => deck.set_keylock(on),
            DeckCmd::Semitones(s) => deck.set_semitones(s),
            DeckCmd::PhaseBend(dir) => deck.phase_bend(dir),
            DeckCmd::PhaseReset => deck.phase_reset(),
            DeckCmd::Reverse(on) => deck.set_reverse(on),
            DeckCmd::Backspin(p) => deck.backspin(p),
            DeckCmd::Slip(on) => deck.set_slip(on),
            DeckCmd::Quantize(q) => deck.set_quantize(q),
            DeckCmd::SetGrid(g) => deck.replace_grid(g),
            DeckCmd::GridStartHere => {
                deck.grid_start_here();
            }
            DeckCmd::BpmBend(b) => deck.set_bpm_bend(b),
            DeckCmd::TapeStopMs(ms) => self.tape_stop_ms[d] = ms.clamp(0.0, 5000.0),
            DeckCmd::StemGain { stem, gain } => deck.set_stem_gain(stem as usize, gain),
        }
    }

    fn handle(&mut self, cmd: Command) {
        match cmd {
            Command::Load { deck, track, start_at_first_cue } => {
                let d = deck as usize;
                if d >= NUM_DECKS {
                    return self.trash(Garbage::Track(track));
                }
                let all_empty = self.decks.iter().all(|x| !x.is_loaded()) && self.mode.iter().all(|m| *m == DeckMode::Track);
                let has_grid = track.grid.is_some();
                self.mode[d] = DeckMode::Track;
                if let Some(old) = self.decks[d].detach_stems() {
                    self.trash(Garbage::Stems(old));
                }
                if let Some(old) = self.decks[d].load(track, start_at_first_cue) {
                    self.trash(Garbage::Track(old));
                }
                if all_empty && has_grid && !self.wallclock {
                    // First track into an empty setup becomes master at native tempo.
                    self.decks[d].set_tempo_fader(0.0);
                    self.owner = ClockOwner::Deck(d as u8);
                    self.sync_before_master[d] = true;
                    self.decks[d].set_sync(false);
                    self.emit(Event::ClockOwner(self.owner));
                } else if has_grid {
                    self.decks[d].set_sync(true);
                }
            }
            Command::Eject { deck } => {
                let d = deck as usize;
                if d < NUM_DECKS {
                    if let Some(old) = self.decks[d].detach_stems() {
                        self.trash(Garbage::Stems(old));
                    }
                    if let Some(old) = self.decks[d].eject() {
                        self.trash(Garbage::Track(old));
                    }
                    self.maintain_owner();
                }
            }
            Command::AttachStems { deck, stems } => {
                let d = deck as usize;
                if d >= NUM_DECKS {
                    return self.trash(Garbage::Stems(stems));
                }
                if let Some(unused) = self.decks[d].attach_stems(stems) {
                    self.trash(Garbage::Stems(unused));
                }
            }
            Command::Deck { deck, cmd } => {
                if (deck as usize) < NUM_DECKS {
                    self.handle_deck(deck as usize, cmd);
                }
            }
            Command::DeckMode { deck, mode } => {
                let d = deck as usize;
                if d < NUM_DECKS && self.mode[d] != mode {
                    if mode == DeckMode::Sampler {
                        if let Some(old) = self.decks[d].detach_stems() {
                            self.trash(Garbage::Stems(old));
                        }
                        if let Some(old) = self.decks[d].eject() {
                            self.trash(Garbage::Track(old));
                        }
                    } else {
                        self.samplers[d].stop();
                    }
                    self.mode[d] = mode;
                    self.maintain_owner();
                }
            }
            Command::Sampler { deck, cmd } => {
                let d = deck as usize;
                if d >= NUM_DECKS {
                    return;
                }
                let q = self.global_quantize;
                let beat = self.clock_beat;
                let s = &mut self.samplers[d];
                match cmd {
                    SamplerCmd::Toggle => s.toggle(beat, q),
                    SamplerCmd::Play => s.play(beat, q),
                    SamplerCmd::Stop => s.stop(),
                    SamplerCmd::Trigger { lane, velocity } => s.trigger(lane as usize, velocity),
                    SamplerCmd::Lane { lane, params } => {
                        if (lane as usize) < LANES {
                            s.lanes[lane as usize] = params;
                        }
                    }
                    SamplerCmd::Step { pattern, lane, step, value } => {
                        if let Some(p) = s.patterns.get_mut(pattern as usize) {
                            if let Some(l) = p.get_mut(lane as usize) {
                                if let Some(st) = l.get_mut(step as usize) {
                                    *st = value;
                                }
                            }
                        }
                    }
                    SamplerCmd::Pattern(p) => s.pattern = (p as usize).min(crate::sampler::PATTERNS - 1),
                    SamplerCmd::Swing(v) => s.swing = v.clamp(0.0, 1.0),
                    SamplerCmd::GlobalPitch(v) => s.global_pitch = v.clamp(-12.0, 12.0),
                    SamplerCmd::Speed(v) => s.speed = v,
                }
            }
            Command::SamplerSample { deck, lane, audio } => {
                let d = deck as usize;
                if d < NUM_DECKS {
                    if let Some(old) = self.samplers[d].set_sample(lane as usize, audio) {
                        self.trash(Garbage::Audio(old));
                    }
                } else if let Some(a) = audio {
                    self.trash(Garbage::Audio(a));
                }
            }
            Command::Channel { ch, params } => {
                let c = ch as usize;
                if c < NUM_DECKS {
                    let before = self.channels[c].params.volume;
                    self.channels[c].set_params(params);
                    if self.fader_start[c] && self.mode[c] == DeckMode::Track {
                        let ctx = self.deck_ctx(c);
                        if before <= 0.0 && params.volume > 0.0 {
                            self.decks[c].play(&ctx);
                        } else if before > 0.0 && params.volume <= 0.0 {
                            self.decks[c].pause();
                        }
                    }
                }
            }
            Command::FaderStart { ch, on } => {
                if let Some(f) = self.fader_start.get_mut(ch as usize) {
                    *f = on;
                }
            }
            Command::XfAssign { ch, assign } => {
                if let Some(a) = self.xf_assign.get_mut(ch as usize) {
                    *a = assign;
                }
            }
            Command::MixMode(m) => self.mix_mode = m,
            Command::FaderCurve(c) => self.curve = c,
            Command::Crossovers { low_hz, high_hz } => {
                for ch in self.channels.iter_mut() {
                    ch.set_crossovers(low_hz, high_hz);
                }
            }
            Command::ExtStages(s) => {
                for ch in self.channels.iter_mut() {
                    ch.ext_stages = s;
                }
            }
            Command::FxInsert { ch, slot, effect, pos, on, mix } => {
                let c = ch as usize;
                if c >= NUM_DECKS || slot as usize >= MAX_SLOTS {
                    let s = FxSlot::new(effect, pos, self.cfg.sample_rate, 0);
                    return self.trash(Garbage::Fx(s));
                }
                let mut s = FxSlot::new(effect, pos, self.cfg.sample_rate, self.cfg.max_latency);
                s.on = on;
                s.mix = mix;
                if let Some(old) = self.channels[c].fx.install(slot as usize, s) {
                    self.trash(Garbage::Fx(old));
                }
                self.update_latency();
            }
            Command::FxRemove { ch, slot } => {
                if let Some(c) = self.channels.get_mut(ch as usize) {
                    if let Some(old) = c.fx.remove(slot as usize) {
                        self.trash(Garbage::Fx(old));
                    }
                }
                self.update_latency();
            }
            Command::FxMove { ch, from, to } => {
                if let Some(c) = self.channels.get_mut(ch as usize) {
                    c.fx.move_slot(from as usize, to as usize);
                }
            }
            Command::FxSlot { ch, slot, pos, on, mix } => {
                if let Some(c) = self.channels.get_mut(ch as usize) {
                    if let Some(Some(s)) = c.fx.slots.get_mut(slot as usize) {
                        s.pos = pos;
                        s.on = on;
                        s.mix = mix.clamp(0.0, 1.0);
                    }
                }
                self.update_latency();
            }
            Command::FxParam { ch, slot, param, value } => {
                if let Some(c) = self.channels.get_mut(ch as usize) {
                    c.fx.set_param(slot as usize, param as usize, value);
                }
            }
            Command::FxBypassAll { ch, on } => {
                if let Some(c) = self.channels.get_mut(ch as usize) {
                    c.fx.bypass_all = on;
                }
            }
            Command::FxMacro { ch, idx, value } => {
                if let Some(c) = self.channels.get_mut(ch as usize) {
                    c.fx.set_macro(idx as usize, value);
                }
            }
            Command::FxMacroTargets { ch, idx, targets } => {
                if let Some(c) = self.channels.get_mut(ch as usize) {
                    if let Some(m) = c.fx.macros.get_mut(idx as usize) {
                        m.targets = targets;
                    }
                }
            }
            Command::FxLatency { ch, slot, samples } => {
                if let Some(c) = self.channels.get_mut(ch as usize) {
                    if let Some(Some(s)) = c.fx.slots.get_mut(slot as usize) {
                        s.set_latency(samples as usize);
                    }
                }
                self.update_latency();
            }
            Command::FxRearm { ch, slot } => {
                if let Some(c) = self.channels.get_mut(ch as usize) {
                    if let Some(Some(s)) = c.fx.slots.get_mut(slot as usize) {
                        s.health = SlotHealth::Ok;
                        s.effect.reset();
                    }
                }
                self.update_latency();
            }
            Command::MasterGain(g) => self.master_gain = g.max(0.0),
            Command::Limiter(on) => self.limiter.enabled = on,
            Command::CueGain(g) => self.cue_gain = g.max(0.0),
            Command::CueMix(m) => self.cue_mix = m.clamp(0.0, 1.0),
            Command::SplitCue(on) => self.split_cue = on,
            Command::SetBpm(bpm) => {
                let bpm = bpm.clamp(20.0, 999.0);
                match self.owner {
                    ClockOwner::Header => self.header_bpm = bpm,
                    ClockOwner::Deck(d) => self.decks[d as usize].set_bpm(bpm),
                }
            }
            Command::HeaderBend(b) => {
                self.header_bend = b;
                if let ClockOwner::Deck(d) = self.owner {
                    self.decks[d as usize].set_bpm_bend(b);
                }
            }
            Command::HeaderMaster => {
                match self.owner {
                    ClockOwner::Header => {
                        if !self.wallclock {
                            if let Some(i) = (0..NUM_DECKS).find(|&i| {
                                self.mode[i] == DeckMode::Track
                                    && self.decks[i].is_playing()
                                    && self.decks[i].sync()
                                    && self.decks[i].grid().is_some()
                            }) {
                                self.set_owner(ClockOwner::Deck(i as u8));
                            }
                        }
                    }
                    ClockOwner::Deck(_) => self.set_owner(ClockOwner::Header),
                }
            }
            Command::Wallclock(on) => {
                self.wallclock = on;
                self.maintain_owner();
            }
            Command::GlobalQuantize(q) => self.global_quantize = q,
            Command::Metronome { on, gain, to_master } => {
                self.metronome.on = on;
                self.metronome.gain = gain;
                self.metronome.to_master = to_master;
            }
            Command::StartRecording(p) => {
                if let Some(old) = self.recorder.replace(p) {
                    self.trash(Garbage::Recorder(old));
                }
            }
            Command::StopRecording => {
                if let Some(old) = self.recorder.take() {
                    self.trash(Garbage::Recorder(old));
                }
            }
        }
    }

    /// Render `n` frames (n ≤ max_block) into the internal bus buffers.
    pub fn process(&mut self, n: usize) {
        let t0 = Instant::now();
        let n = n.min(self.cfg.max_block);
        let sr = self.cfg.sample_rate as f64;

        while let Ok(cmd) = self.commands.pop() {
            self.handle(cmd);
        }
        self.maintain_owner();

        // ---- clock: lock phase to the master deck
        let bpm = self.clock_bpm();
        if let ClockOwner::Deck(d) = self.owner {
            let deck = &self.decks[d as usize];
            if deck.is_playing() {
                if let Some(b) = deck.beat() {
                    let k = (self.clock_beat - b).round();
                    self.clock_beat = b + k;
                }
            }
        }
        let beat0 = self.clock_beat;
        let bpf = bpm / 60.0 / sr;
        let fx_ctx = FxCtx { sample_rate: self.cfg.sample_rate, bpm, beat: beat0, beats_per_frame: bpf };

        let master = &mut self.master_buf[..n];
        master.fill([0.0; 2]);
        let cue = &mut self.cue_buf[..n];
        cue.fill([0.0; 2]);
        self.pair_used = [false; MAX_PAIRS];

        let any_solo = self.channels.iter().any(|c| c.params.solo);
        let gains = mix_gains(self.mix_mode, &self.xf_assign);
        let mut any_cue = false;

        for d in 0..NUM_DECKS {
            let buf = &mut self.deck_buf[..n];
            match self.mode[d] {
                DeckMode::Track => {
                    let ctx = DeckCtx {
                        out_rate: sr,
                        clock_bpm: bpm,
                        clock_beat: beat0,
                        global_quantize: self.global_quantize,
                        is_clock_owner: self.owner == ClockOwner::Deck(d as u8),
                        tape_stop_ms: self.tape_stop_ms[d],
                    };
                    self.decks[d].process(buf, &ctx);
                    let gain = self.decks[d].track().map(|t| t.gain).unwrap_or(1.0);
                    if gain != 1.0 {
                        for f in buf.iter_mut() {
                            f[0] *= gain;
                            f[1] *= gain;
                        }
                    }
                }
                DeckMode::Sampler => self.samplers[d].process(buf, beat0, bpf),
            }

            let ch = &mut self.channels[d];
            let audible = !ch.params.mute && (!any_solo || ch.params.solo);
            ch.mix_gain.set(gains[d]);
            let output = ch.params.output;
            if let Some(tap) = ch.process(buf, &fx_ctx, self.curve, audible) {
                any_cue = true;
                for (c, t) in cue.iter_mut().zip(tap) {
                    c[0] += t[0];
                    c[1] += t[1];
                }
            }
            match output {
                Output::Master => {
                    for (m, x) in master.iter_mut().zip(buf.iter()) {
                        m[0] += x[0];
                        m[1] += x[1];
                    }
                }
                Output::Pair(p) if (p as usize) < MAX_PAIRS => {
                    let p = p as usize;
                    let dst = &mut self.pair_bufs[p][..n];
                    if !self.pair_used[p] {
                        dst.fill([0.0; 2]);
                        self.pair_used[p] = true;
                    }
                    for (m, x) in dst.iter_mut().zip(buf.iter()) {
                        m[0] += x[0];
                        m[1] += x[1];
                    }
                }
                _ => {}
            }

            self.decks[d].take_events(|e| {
                let _ = self.events.push(Event::Deck { deck: d as u8, event: e });
            });
        }

        // ---- metronome (generated, then mixed after the limiter section)
        let mut beat_events: [Option<(i64, u8)>; 4] = [None; 4];
        let mut nbe = 0;
        {
            let m = &mut self.metronome;
            for (i, f) in self.deck_buf[..n].iter_mut().enumerate() {
                let b = beat0 + i as f64 * bpf;
                let prev = beat0 + (i as f64 - 1.0) * bpf;
                if b.floor() != prev.floor() || (i == 0 && self.frames == 0) {
                    let bi = b.floor() as i64;
                    let in_bar = bi.rem_euclid(4) as u8;
                    m.env = 1.0;
                    m.phase = 0.0;
                    m.freq = if in_bar == 0 { 1760.0 } else { 1100.0 };
                    if nbe < beat_events.len() {
                        beat_events[nbe] = Some((bi.div_euclid(4) + 1, in_bar + 1));
                        nbe += 1;
                    }
                }
                let s = if m.on && m.env > 0.001 {
                    m.phase += m.freq / sr as f32;
                    m.env *= 0.9985;
                    (m.phase * std::f32::consts::TAU).sin() * m.env * m.gain * 0.5
                } else {
                    0.0
                };
                *f = [s, s];
            }
        }
        for e in beat_events.iter().flatten() {
            let _ = self.events.push(Event::Beat { bar: e.0, beat: e.1 });
        }

        let master = &mut self.master_buf[..n];
        let met = &self.deck_buf[..n];
        if self.metronome.on && self.metronome.to_master {
            for (m, x) in master.iter_mut().zip(met) {
                m[0] += x[0];
                m[1] += x[1];
            }
        }

        // Limiter runs before master volume (−0.3 dBFS ceiling, 1.5 ms lookahead).
        self.limiter.process(master);
        let mg = self.master_gain;
        for f in master.iter_mut() {
            f[0] *= mg;
            f[1] *= mg;
        }
        self.master_meter.process(master, 0.9);

        // ---- cue / headphones
        let cue = &mut self.cue_buf[..n];
        if self.metronome.on {
            any_cue = true;
            for (c, x) in cue.iter_mut().zip(met) {
                c[0] += x[0];
                c[1] += x[1];
            }
        }
        let mix = if any_cue { self.cue_mix } else { 1.0 };
        let cg = self.cue_gain;
        for (c, m) in cue.iter_mut().zip(master.iter()) {
            if self.split_cue {
                // Cue (mono) left, master (mono) right.
                let cm = (c[0] + c[1]) * 0.5;
                let mm = (m[0] + m[1]) * 0.5;
                *c = [cm * cg, mm * cg];
            } else {
                c[0] = (c[0] * (1.0 - mix) + m[0] * mix) * cg;
                c[1] = (c[1] * (1.0 - mix) + m[1] * mix) * cg;
            }
        }
        self.cue_meter.process(cue, 0.9);

        // ---- recording
        if let Some(rec) = self.recorder.as_mut() {
            let mut overflow = false;
            for f in master.iter() {
                if rec.push(*f).is_err() {
                    overflow = true;
                    break;
                }
            }
            if overflow {
                let _ = self.events.push(Event::RecordingOverflow);
            }
        }

        self.clock_beat += n as f64 * bpf;
        self.frames += n as u64;

        // ---- load + snapshot
        let elapsed = t0.elapsed().as_secs_f32();
        let budget = n as f32 / sr as f32;
        self.load = self.load * 0.9 + (elapsed / budget) * 0.1;
        self.since_snapshot += n as u64;
        if self.since_snapshot >= self.snapshot_every {
            self.since_snapshot = 0;
            let snap = self.snapshot();
            if let Ok(mut s) = self.snapshots.try_lock() {
                *s = snap;
            }
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        let mut s = Snapshot::default();
        for d in 0..NUM_DECKS {
            s.decks[d] = self.decks[d].view(&self.deck_ctx(d));
            s.deck_mode[d] = self.mode[d];
            s.samplers[d] = self.samplers[d].view();
            let ch = &self.channels[d];
            let (pre, post) = ch.meters();
            let (lp, lq) = ch.fx.latency();
            s.channels[d] = ChannelView {
                pre_peak: pre.peak,
                post_peak: post.peak,
                post_rms: post.rms,
                fx: ch.fx.health(),
                fx_latency: (lp as u32, lq as u32),
                mix_gain: ch.mix_gain.target,
            };
        }
        s.master = self.master_meter;
        s.cue = self.cue_meter;
        s.limiter_db = self.limiter.reduction_db;
        s.clock = ClockView { bpm: self.clock_bpm(), beat: self.clock_beat, owner: self.owner, wallclock: self.wallclock };
        s.global_quantize = self.global_quantize;
        s.cpu_load = self.load;
        s.compensation_latency = self.compensation as u32;
        s.recording = self.recorder.is_some();
        s.frames = self.frames;
        s
    }
}

pub fn gain_from_db(db: f32) -> f32 {
    db_to_gain(db)
}
