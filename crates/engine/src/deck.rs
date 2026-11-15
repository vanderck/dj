//! A track deck: transport, tempo, sync, key lock, hot cues, loops, slip.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::dsp::Frame;
use crate::stretch::{Source, Stretcher};
use crate::track::{BeatGrid, Cue, CueKind, StemData, Track, NUM_HOT_CUES, NUM_STEMS};

/// Quantize setting. Values are in beats (4 beats = 1 bar).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, Default)]
pub enum Quantize {
    #[default]
    UseGlobal,
    Off,
    Beats(f64),
}

impl Quantize {
    /// Same index list as the OSC API: 0 global, 1 none, 2 = 2 bars … 8 = 1/32 bar.
    pub fn from_index(i: i32) -> Self {
        match i.clamp(0, 8) {
            0 => Quantize::UseGlobal,
            1 => Quantize::Off,
            n => Quantize::Beats(8.0 / 2f64.powi(n - 2)),
        }
    }

    pub fn resolve(self, global: Option<f64>) -> Option<f64> {
        match self {
            Quantize::UseGlobal => global,
            Quantize::Off => None,
            Quantize::Beats(b) => Some(b),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum TempoRange {
    Pct6,
    #[default]
    Pct10,
    Pct16,
    Pct50,
    /// ±12 semitones: half to double speed (exponential).
    Semitones12,
}

impl TempoRange {
    pub fn speed(self, fader: f64) -> f64 {
        let f = fader.clamp(-1.0, 1.0);
        match self {
            TempoRange::Pct6 => 1.0 + 0.06 * f,
            TempoRange::Pct10 => 1.0 + 0.10 * f,
            TempoRange::Pct16 => 1.0 + 0.16 * f,
            TempoRange::Pct50 => 1.0 + 0.50 * f,
            TempoRange::Semitones12 => 2f64.powf(f),
        }
    }

    pub fn fader_for_speed(self, speed: f64) -> f64 {
        let f = match self {
            TempoRange::Pct6 => (speed - 1.0) / 0.06,
            TempoRange::Pct10 => (speed - 1.0) / 0.10,
            TempoRange::Pct16 => (speed - 1.0) / 0.16,
            TempoRange::Pct50 => (speed - 1.0) / 0.50,
            TempoRange::Semitones12 => speed.max(1e-6).log2(),
        };
        f.clamp(-1.0, 1.0)
    }

    pub fn label(self) -> &'static str {
        match self {
            TempoRange::Pct6 => "±6%",
            TempoRange::Pct10 => "±10%",
            TempoRange::Pct16 => "±16%",
            TempoRange::Pct50 => "±50%",
            TempoRange::Semitones12 => "±12st",
        }
    }
}

/// Context handed to the deck each block by the engine.
#[derive(Clone, Copy, Debug)]
pub struct DeckCtx {
    pub out_rate: f64,
    pub clock_bpm: f64,
    /// Master clock beat at the start of the block.
    pub clock_beat: f64,
    pub global_quantize: Option<f64>,
    pub is_clock_owner: bool,
    pub tape_stop_ms: f64,
}

impl DeckCtx {
    pub fn clock_beats_per_frame(&self) -> f64 {
        self.clock_bpm / 60.0 / self.out_rate
    }
}

/// Notifications produced by a deck (forwarded to UI / OSC feedback).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DeckEvent {
    PlayState { playing: bool },
    /// A hot cue was created in Perform. The UI decides whether to persist it.
    CueSet { idx: u8, secs: f64, end_secs: Option<f64> },
    CueDeleted { idx: u8 },
    /// A (possibly quantized) cue actually launched.
    CueFire { idx: u8 },
    EndOfTrack,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum PendingAction {
    Play,
    JumpCue(u8),
    CueLoop(u8),
}

#[derive(Clone, Copy, Debug)]
struct Pending {
    action: PendingAction,
    /// Fire when the reference beat reaches this value.
    at_beat: f64,
    /// Reference is the master clock (true) or the deck's own grid (false).
    on_clock: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LoopRegion {
    pub start: f64,
    pub end: f64,
}

impl LoopRegion {
    pub fn len(&self) -> f64 {
        self.end - self.start
    }
}

#[derive(Clone, Copy, Debug)]
struct Roll {
    saved_loop: Option<LoopRegion>,
    saved_loop_on: bool,
}

struct Reader<'a> {
    frames: &'a [Frame],
    looping: Option<LoopRegion>,
    /// Separated stems and the current per-stem gains; `None` plays the plain mix.
    stems: Option<(&'a StemData, [f32; NUM_STEMS])>,
}

impl Source for Reader<'_> {
    #[inline]
    fn frame(&self, mut i: i64) -> Frame {
        if let Some(l) = self.looping {
            let (s, e) = (l.start as i64, l.end as i64);
            let len = (e - s).max(1);
            if i >= e {
                i = s + (i - e) % len;
            }
        }
        if i < 0 || i as usize >= self.frames.len() {
            [0.0; 2]
        } else if let Some((stems, gains)) = &self.stems {
            stems.mix(i as usize, self.frames[i as usize], gains)
        } else {
            self.frames[i as usize]
        }
    }
}

const XFADE_FRAMES: u32 = 192; // ~4 ms blend when switching render modes

/// Public, copyable view of a deck for snapshots.
#[derive(Clone, Copy, Debug, Default)]
pub struct DeckView {
    pub track_id: u64,
    pub loaded: bool,
    pub playing: bool,
    pub play_pending: bool,
    pub position_secs: f64,
    pub duration_secs: f64,
    pub beat: f64,
    pub has_grid: bool,
    pub track_bpm: f64,
    pub bpm: f64,
    pub speed: f64,
    pub tempo_fader: f64,
    pub tempo_range: TempoRange,
    pub speed_mult: f64,
    pub sync: bool,
    pub keylock: bool,
    pub semitones: f64,
    pub reverse: bool,
    pub slip: bool,
    pub slip_engaged: bool,
    pub momentary_cues: bool,
    pub quantize: Quantize,
    pub loop_on: bool,
    pub loop_start_secs: f64,
    pub loop_end_secs: f64,
    pub loop_beats: f64,
    pub cue_point_secs: f64,
    pub hot_cues: [Option<f64>; NUM_HOT_CUES],
    pub hot_cue_is_loop: [bool; NUM_HOT_CUES],
    pub phase_offset: f64,
    pub phase_error: f64,
    /// Stems have been separated and attached for the loaded track.
    pub stems_ready: bool,
    /// Requested per-stem gains in [`crate::track::Stem`] order.
    pub stem_gains: [f32; NUM_STEMS],
}

pub struct Deck {
    track: Option<Arc<Track>>,
    grid_override: Option<BeatGrid>,
    out_rate: f64,
    step: f64,
    pos: f64,
    playing: bool,
    motor: f64,
    tempo_fader: f64,
    tempo_range: TempoRange,
    speed_mult: f64,
    reverse: bool,
    keylock: bool,
    semitones: f64,
    sync: bool,
    phase_offset: f64,
    phase_bend: f64,
    bpm_bend: f64,
    quantize: Quantize,
    loop_region: Option<LoopRegion>,
    loop_on: bool,
    pub default_loop_beats: f64,
    slip: bool,
    slip_pos: f64,
    was_slip_engaged: bool,
    roll: Option<Roll>,
    momentary_cues: bool,
    held_cue: Option<u8>,
    cue_point: f64,
    cue_preview: bool,
    hot_cues: [Option<LoopRegionOrPoint>; NUM_HOT_CUES],
    pending: Option<Pending>,
    backspin: Option<f64>,
    backspin_held: bool,
    stretcher: Stretcher,
    stretch_active: bool,
    xfade: u32,
    resync_requested: bool,
    last_speed: f64,
    last_phase_error: f64,
    events: [Option<DeckEvent>; 8],
    n_events: usize,
    stems: Option<Arc<StemData>>,
    /// Requested per-stem gains.
    stem_target: [f32; NUM_STEMS],
    /// Smoothed per-stem gains actually applied (avoids clicks on mute).
    stem_gain: [f32; NUM_STEMS],
}

/// Time constant for stem gain changes.
const STEM_SMOOTH_SECS: f64 = 0.004;

#[derive(Clone, Copy, Debug)]
struct LoopRegionOrPoint {
    start: f64,
    end: Option<f64>,
}

impl Deck {
    pub fn new(out_rate: f64) -> Self {
        Self {
            track: None,
            grid_override: None,
            out_rate,
            step: 1.0,
            pos: 0.0,
            playing: false,
            motor: 0.0,
            tempo_fader: 0.0,
            tempo_range: TempoRange::default(),
            speed_mult: 1.0,
            reverse: false,
            keylock: true,
            semitones: 0.0,
            sync: false,
            phase_offset: 0.0,
            phase_bend: 0.0,
            bpm_bend: 0.0,
            quantize: Quantize::UseGlobal,
            loop_region: None,
            loop_on: false,
            default_loop_beats: 16.0,
            slip: false,
            slip_pos: 0.0,
            was_slip_engaged: false,
            roll: None,
            momentary_cues: false,
            held_cue: None,
            cue_point: 0.0,
            cue_preview: false,
            hot_cues: [None; NUM_HOT_CUES],
            pending: None,
            backspin: None,
            backspin_held: false,
            stretcher: Stretcher::new(out_rate as f32),
            stretch_active: false,
            xfade: 0,
            resync_requested: false,
            last_speed: 1.0,
            last_phase_error: 0.0,
            events: [None; 8],
            n_events: 0,
            stems: None,
            stem_target: [1.0; NUM_STEMS],
            stem_gain: [1.0; NUM_STEMS],
        }
    }

    // ------------------------------------------------------------ stems

    /// Attach separated stems. Accepted only if they belong to the loaded track; returns whatever
    /// must be released off the audio thread (the previous stems, or the rejected ones).
    pub fn attach_stems(&mut self, stems: Arc<StemData>) -> Option<Arc<StemData>> {
        match &self.track {
            Some(t) if t.id == stems.track_id => self.stems.replace(stems),
            _ => Some(stems),
        }
    }

    /// Remove the stems (on load / eject); the caller disposes of them off the audio thread.
    pub fn detach_stems(&mut self) -> Option<Arc<StemData>> {
        self.stems.take()
    }

    pub fn stems_ready(&self) -> bool {
        self.stems.is_some()
    }

    pub fn set_stem_gain(&mut self, stem: usize, gain: f32) {
        if let Some(g) = self.stem_target.get_mut(stem) {
            *g = gain.clamp(0.0, 2.0);
        }
    }

    pub fn stem_gains(&self) -> [f32; NUM_STEMS] {
        self.stem_target
    }

    fn emit(&mut self, e: DeckEvent) {
        if self.n_events < self.events.len() {
            self.events[self.n_events] = Some(e);
            self.n_events += 1;
        }
    }

    /// Drain events produced since the last call.
    pub fn take_events(&mut self, mut f: impl FnMut(DeckEvent)) {
        for i in 0..self.n_events {
            if let Some(e) = self.events[i].take() {
                f(e);
            }
        }
        self.n_events = 0;
    }

    // ------------------------------------------------------------ accessors

    pub fn track(&self) -> Option<&Arc<Track>> {
        self.track.as_ref()
    }

    pub fn is_loaded(&self) -> bool {
        self.track.is_some()
    }

    pub fn is_playing(&self) -> bool {
        self.playing
    }

    pub fn sync(&self) -> bool {
        self.sync
    }

    fn src_rate(&self) -> f64 {
        self.track.as_ref().map(|t| t.audio.sample_rate as f64).unwrap_or(self.out_rate)
    }

    fn len_frames(&self) -> f64 {
        self.track.as_ref().map(|t| t.audio.frames.len() as f64).unwrap_or(0.0)
    }

    pub fn grid(&self) -> Option<BeatGrid> {
        self.grid_override.or_else(|| self.track.as_ref().and_then(|t| t.grid))
    }

    fn frames_per_beat(&self) -> Option<f64> {
        self.grid().map(|g| g.seconds_per_beat() * self.src_rate())
    }

    pub fn position_secs(&self) -> f64 {
        self.pos / self.src_rate()
    }

    /// Fractional beat at the current position (beat 0 = Grid Start).
    pub fn beat(&self) -> Option<f64> {
        self.grid().map(|g| g.beat_at(self.position_secs()))
    }

    fn beat_at_frame(&self, pos: f64) -> Option<f64> {
        self.grid().map(|g| g.beat_at(pos / self.src_rate()))
    }

    fn frame_at_beat(&self, beat: f64) -> Option<f64> {
        self.grid().map(|g| g.secs_at_beat(beat) * self.src_rate())
    }

    /// Base speed (no motor, no phase correction), given the clock.
    fn base_speed(&self, ctx: &DeckCtx) -> f64 {
        let follower = self.sync && !ctx.is_clock_owner;
        match (follower, self.grid()) {
            (true, Some(g)) => ctx.clock_bpm / g.bpm * self.speed_mult,
            _ => {
                let s = self.tempo_range.speed(self.tempo_fader) * self.speed_mult;
                if ctx.is_clock_owner && self.bpm_bend != 0.0 {
                    if let Some(g) = self.grid() {
                        return s + self.bpm_bend / g.bpm;
                    }
                }
                s
            }
        }
    }

    /// The deck's effective BPM if it were to lead the clock.
    pub fn effective_bpm(&self) -> Option<f64> {
        let g = self.grid()?;
        let mut s = self.tempo_range.speed(self.tempo_fader) * self.speed_mult;
        s += self.bpm_bend / g.bpm;
        Some(g.bpm * s)
    }

    fn quantize_beats(&self, ctx: &DeckCtx) -> Option<f64> {
        self.quantize.resolve(ctx.global_quantize)
    }

    pub fn view(&self, ctx: &DeckCtx) -> DeckView {
        let sr = self.src_rate();
        let grid = self.grid();
        let mut hot = [None; NUM_HOT_CUES];
        let mut hot_loop = [false; NUM_HOT_CUES];
        for (i, c) in self.hot_cues.iter().enumerate() {
            if let Some(c) = c {
                hot[i] = Some(c.start / sr);
                hot_loop[i] = c.end.is_some();
            }
        }
        let speed = self.base_speed(ctx);
        let lr = self.loop_region.unwrap_or(LoopRegion { start: 0.0, end: 0.0 });
        DeckView {
            track_id: self.track.as_ref().map(|t| t.id).unwrap_or(0),
            loaded: self.track.is_some(),
            playing: self.playing,
            play_pending: matches!(self.pending, Some(Pending { action: PendingAction::Play, .. })),
            position_secs: self.pos / sr,
            duration_secs: self.len_frames() / sr,
            beat: self.beat().unwrap_or(0.0),
            has_grid: grid.is_some(),
            track_bpm: grid.map(|g| g.bpm).unwrap_or(0.0),
            bpm: grid.map(|g| g.bpm * speed).unwrap_or(0.0),
            speed,
            tempo_fader: self.tempo_fader,
            tempo_range: self.tempo_range,
            speed_mult: self.speed_mult,
            sync: self.sync,
            keylock: self.keylock,
            semitones: self.semitones,
            reverse: self.reverse,
            slip: self.slip,
            slip_engaged: self.was_slip_engaged,
            momentary_cues: self.momentary_cues,
            quantize: self.quantize,
            loop_on: self.loop_on,
            loop_start_secs: lr.start / sr,
            loop_end_secs: lr.end / sr,
            loop_beats: self.frames_per_beat().map(|f| lr.len() / f).unwrap_or(0.0),
            cue_point_secs: self.cue_point / sr,
            hot_cues: hot,
            hot_cue_is_loop: hot_loop,
            phase_offset: self.phase_offset,
            phase_error: self.last_phase_error,
            stems_ready: self.stems.is_some(),
            stem_gains: self.stem_target,
        }
    }

    // ------------------------------------------------------------ loading

    /// Load a track. Returns the previous track so the caller can dispose of
    /// it off the audio thread.
    pub fn load(&mut self, track: Arc<Track>, start_at_first_cue: bool) -> Option<Arc<Track>> {
        let sr = track.audio.sample_rate as f64;
        self.step = sr / self.out_rate;
        self.hot_cues = [None; NUM_HOT_CUES];
        for (i, c) in track.cues.iter().enumerate() {
            if let Some(c) = c {
                self.hot_cues[i] = Some(LoopRegionOrPoint {
                    start: c.secs * sr,
                    end: if c.kind == CueKind::Loop { c.end_secs.map(|e| e * sr) } else { None },
                });
            }
        }
        let first_cue = track
            .cues
            .iter()
            .flatten()
            .map(|c| c.secs)
            .fold(f64::INFINITY, f64::min);
        self.pos = if start_at_first_cue && first_cue.is_finite() {
            first_cue * sr
        } else {
            0.0
        };
        self.cue_point = self.pos;
        self.playing = false;
        self.motor = 0.0;
        self.loop_region = None;
        self.loop_on = false;
        self.roll = None;
        self.pending = None;
        self.backspin = None;
        self.reverse = false;
        self.phase_offset = 0.0;
        self.semitones = 0.0;
        self.stretcher.reset();
        self.slip_pos = self.pos;
        self.grid_override = None;
        // A new track starts with every stem on.
        self.stem_target = [1.0; NUM_STEMS];
        self.stem_gain = [1.0; NUM_STEMS];
        self.track.replace(track)
    }

    pub fn eject(&mut self) -> Option<Arc<Track>> {
        self.playing = false;
        self.motor = 0.0;
        self.pending = None;
        self.loop_on = false;
        self.loop_region = None;
        self.hot_cues = [None; NUM_HOT_CUES];
        if self.playing {
            self.emit(DeckEvent::PlayState { playing: false });
        }
        self.track.take()
    }

    // ------------------------------------------------------------ transport

    fn boundary(&self, ctx: &DeckCtx, on_clock: bool) -> Option<f64> {
        let q = self.quantize_beats(ctx)?;
        let b = if on_clock { ctx.clock_beat } else { self.beat()? };
        let tol = 0.002;
        let next = ((b - tol) / q).ceil() * q;
        Some(next.max(b))
    }

    fn schedule(&mut self, ctx: &DeckCtx, action: PendingAction, on_clock: bool) -> bool {
        match self.boundary(ctx, on_clock) {
            Some(at) if (at - if on_clock { ctx.clock_beat } else { self.beat().unwrap_or(at) }) > 0.002 => {
                self.pending = Some(Pending { action, at_beat: at, on_clock });
                true
            }
            _ => false,
        }
    }

    pub fn play(&mut self, ctx: &DeckCtx) {
        if self.track.is_none() || self.playing {
            return;
        }
        // Launch on the master clock's quantize grid when anything else is running.
        if !self.schedule(ctx, PendingAction::Play, true) {
            self.start_now(ctx);
        }
    }

    fn start_now(&mut self, ctx: &DeckCtx) {
        if self.pos >= self.len_frames() {
            return;
        }
        self.playing = true;
        self.pending = None;
        self.cue_preview = false;
        if self.sync && !ctx.is_clock_owner {
            self.resync_requested = true;
        }
        self.emit(DeckEvent::PlayState { playing: true });
    }

    /// Pause immediately (tape stop, no quantize).
    pub fn pause(&mut self) {
        if matches!(self.pending, Some(Pending { action: PendingAction::Play, .. })) {
            self.pending = None;
        }
        if self.playing {
            self.playing = false;
            self.emit(DeckEvent::PlayState { playing: false });
        }
    }

    pub fn toggle_play(&mut self, ctx: &DeckCtx) {
        if self.playing || self.pending.is_some() {
            self.pause();
        } else {
            self.play(ctx);
        }
    }

    pub fn set_playing(&mut self, on: bool, ctx: &DeckCtx) {
        if on {
            self.play(ctx)
        } else {
            self.pause()
        }
    }

    /// CDJ-style CUE button.
    pub fn cue(&mut self, pressed: bool, ctx: &DeckCtx) {
        if self.track.is_none() {
            return;
        }
        if pressed {
            if self.playing && !self.cue_preview {
                self.jump_to(self.cue_point);
                self.pause();
                self.motor = 0.0;
            } else if (self.pos - self.cue_point).abs() < self.src_rate() * 0.01 {
                self.cue_preview = true;
                self.start_now(ctx);
            } else {
                self.cue_point = self.snap(self.pos, ctx);
                self.jump_to(self.cue_point);
            }
        } else if self.cue_preview {
            self.cue_preview = false;
            self.pause();
            self.motor = 0.0;
            self.jump_to(self.cue_point);
        }
    }

    pub fn jump_to_grid_start(&mut self) {
        if let Some(g) = self.grid() {
            self.jump_to(g.anchor_secs * self.src_rate());
        }
    }

    fn snap(&self, pos: f64, ctx: &DeckCtx) -> f64 {
        if self.quantize_beats(ctx).is_none() {
            return pos;
        }
        match (self.beat_at_frame(pos), self.frames_per_beat()) {
            (Some(b), Some(_)) => self.frame_at_beat(b.round()).unwrap_or(pos),
            _ => pos,
        }
    }

    fn jump_to(&mut self, pos: f64) {
        self.pos = pos.clamp(0.0, self.len_frames());
        self.stretcher.reset();
        self.xfade = XFADE_FRAMES;
    }

    pub fn seek_fraction(&mut self, frac: f64, ctx: &DeckCtx) {
        let len = self.len_frames();
        let mut p = frac.clamp(0.0, 1.0) * len;
        if !self.playing {
            p = self.snap(p, ctx);
        } else if self.quantize_beats(ctx).is_some() {
            // Preserve beat phase while playing.
            if let (Some(fpb), Some(cur)) = (self.frames_per_beat(), self.beat()) {
                if let Some(tb) = self.beat_at_frame(p) {
                    let frac_now = cur - cur.floor();
                    p = self.frame_at_beat(tb.floor() + frac_now).unwrap_or(p);
                    let _ = fpb;
                }
            }
        }
        self.jump_to(p);
        if self.slip {
            self.slip_pos = self.pos;
        }
    }

    // ------------------------------------------------------------ hot cues

    pub fn set_momentary_cues(&mut self, on: bool) {
        self.momentary_cues = on;
    }

    /// Press/release a hot cue pad. Sets a cue on an empty pad.
    pub fn hot_cue(&mut self, idx: u8, pressed: bool, ctx: &DeckCtx) {
        let i = idx as usize;
        if i >= NUM_HOT_CUES || self.track.is_none() {
            return;
        }
        if !pressed {
            if self.momentary_cues && self.held_cue == Some(idx) {
                self.held_cue = None;
                if self.slip {
                    self.jump_to(self.slip_pos);
                } else {
                    self.pause();
                    self.motor = 0.0;
                    if let Some(c) = self.hot_cues[i] {
                        self.jump_to(c.start);
                    }
                }
            }
            return;
        }
        match self.hot_cues[i] {
            None => {
                let sr = self.src_rate();
                let cue = if self.loop_on {
                    let l = self.loop_region.unwrap();
                    LoopRegionOrPoint { start: l.start, end: Some(l.end) }
                } else {
                    LoopRegionOrPoint { start: self.snap(self.pos, ctx), end: None }
                };
                self.hot_cues[i] = Some(cue);
                self.emit(DeckEvent::CueSet { idx, secs: cue.start / sr, end_secs: cue.end.map(|e| e / sr) });
            }
            Some(_) => {
                self.held_cue = Some(idx);
                if self.playing {
                    let on_clock = self.sync || ctx.is_clock_owner;
                    if !self.schedule(ctx, PendingAction::JumpCue(idx), on_clock) {
                        self.fire_cue(idx, false, ctx);
                    }
                } else {
                    self.fire_cue(idx, false, ctx);
                    self.play(ctx);
                }
            }
        }
    }

    fn fire_cue(&mut self, idx: u8, force_loop: bool, ctx: &DeckCtx) {
        let Some(c) = self.hot_cues[idx as usize] else { return };
        if self.slip && self.playing {
            // Keep the underlying playhead running.
            if !self.was_slip_engaged {
                self.slip_pos = self.pos;
            }
        }
        self.jump_to(c.start);
        if let Some(end) = c.end {
            self.loop_region = Some(LoopRegion { start: c.start, end });
            self.loop_on = true;
        } else if force_loop {
            // Loop from this cue to the next cue in time (or end of track).
            let next = self
                .hot_cues
                .iter()
                .flatten()
                .map(|o| o.start)
                .filter(|&s| s > c.start + 1.0)
                .fold(self.len_frames(), f64::min);
            self.loop_region = Some(LoopRegion { start: c.start, end: next });
            self.loop_on = true;
        }
        let _ = ctx;
        self.emit(DeckEvent::CueFire { idx });
    }

    /// Launch cue `idx` with LOOP on from it to the next cue.
    pub fn cue_loop(&mut self, idx: u8, ctx: &DeckCtx) {
        if self.hot_cues.get(idx as usize).copied().flatten().is_none() {
            return;
        }
        if self.playing {
            let on_clock = self.sync || ctx.is_clock_owner;
            if !self.schedule(ctx, PendingAction::CueLoop(idx), on_clock) {
                self.fire_cue(idx, true, ctx);
            }
        } else {
            self.fire_cue(idx, true, ctx);
            self.play(ctx);
        }
    }

    pub fn delete_hot_cue(&mut self, idx: u8) {
        if let Some(c) = self.hot_cues.get_mut(idx as usize) {
            if c.take().is_some() {
                self.emit(DeckEvent::CueDeleted { idx });
            }
        }
    }

    /// Replace a cue from the UI/editor (no event).
    pub fn set_hot_cue(&mut self, idx: u8, cue: Option<&Cue>) {
        let sr = self.src_rate();
        if let Some(slot) = self.hot_cues.get_mut(idx as usize) {
            *slot = cue.map(|c| LoopRegionOrPoint {
                start: c.secs * sr,
                end: if c.kind == CueKind::Loop { c.end_secs.map(|e| e * sr) } else { None },
            });
        }
    }

    // ------------------------------------------------------------ loops

    pub fn loop_toggle(&mut self, ctx: &DeckCtx) {
        if self.loop_on {
            self.loop_exit();
        } else {
            self.loop_beats(self.default_loop_beats, ctx);
        }
    }

    /// Start a loop of `beats` length at the current (quantized) position.
    pub fn loop_beats(&mut self, beats: f64, ctx: &DeckCtx) {
        let Some(fpb) = self.frames_per_beat() else {
            // No grid: assume 120 BPM so loops still work.
            let fpb = self.src_rate() * 0.5;
            self.loop_region = Some(LoopRegion { start: self.pos, end: self.pos + beats * fpb });
            self.loop_on = true;
            return;
        };
        let start = if self.quantize_beats(ctx).is_some() {
            let b = self.beat().unwrap();
            let unit = beats.min(1.0);
            self.frame_at_beat((b / unit).floor() * unit).unwrap()
        } else {
            self.pos
        };
        self.loop_region = Some(LoopRegion { start, end: start + beats * fpb });
        self.loop_on = true;
    }

    pub fn loop_in(&mut self, ctx: &DeckCtx) {
        if self.loop_on {
            self.loop_scale(0.5);
            return;
        }
        let p = self.snap(self.pos, ctx);
        self.loop_region = Some(LoopRegion { start: p, end: p });
    }

    pub fn loop_out(&mut self, ctx: &DeckCtx) {
        if self.loop_on {
            self.loop_scale(2.0);
            return;
        }
        if let Some(mut l) = self.loop_region {
            let e = self.snap(self.pos, ctx);
            if e > l.start + 16.0 {
                l.end = e;
                self.loop_region = Some(l);
                self.loop_on = true;
                if self.pos >= l.end {
                    self.jump_to(l.start);
                }
            }
        }
    }

    pub fn loop_scale(&mut self, factor: f64) {
        if let Some(mut l) = self.loop_region {
            let new_len = (l.len() * factor).max(32.0);
            l.end = l.start + new_len;
            self.loop_region = Some(l);
            if self.loop_on && self.pos >= l.end {
                let rel = (self.pos - l.start) % new_len;
                self.pos = l.start + rel;
                self.stretcher.reset();
            }
        }
    }

    pub fn loop_exit(&mut self) {
        self.loop_on = false;
    }

    /// Free-loop knob: `ms` sets a very short loop in milliseconds.
    pub fn free_loop(&mut self, ms: f64) {
        let len = (ms.clamp(5.0, 2000.0) * 0.001 * self.src_rate()).max(32.0);
        let start = self.loop_region.filter(|_| self.loop_on).map(|l| l.start).unwrap_or(self.pos);
        self.loop_region = Some(LoopRegion { start, end: start + len });
        self.loop_on = true;
    }

    pub fn loop_move(&mut self, beats: f64) {
        let Some(fpb) = self.frames_per_beat() else { return };
        if let Some(mut l) = self.loop_region {
            let d = beats * fpb;
            l.start += d;
            l.end += d;
            self.loop_region = Some(l);
            if self.loop_on {
                self.pos += d;
                self.stretcher.reset();
            }
        }
    }

    /// Hold-to-repeat slip loop ("roll"). `beats` already includes D/T modifiers.
    pub fn slip_loop(&mut self, beats: f64, pressed: bool, ctx: &DeckCtx) {
        if pressed {
            if self.roll.is_none() {
                self.roll = Some(Roll { saved_loop: self.loop_region, saved_loop_on: self.loop_on });
                self.slip_pos = self.pos;
            }
            let Some(fpb) = self.frames_per_beat() else { return };
            let b = self.beat().unwrap();
            let start = if self.quantize_beats(ctx).is_some() || true {
                let unit = beats.min(1.0);
                self.frame_at_beat((b / unit).floor() * unit).unwrap()
            } else {
                self.pos
            };
            self.loop_region = Some(LoopRegion { start, end: start + beats * fpb });
            self.loop_on = true;
        } else if let Some(r) = self.roll.take() {
            self.loop_region = r.saved_loop;
            self.loop_on = r.saved_loop_on;
            self.jump_to(self.slip_pos);
        }
    }

    // ------------------------------------------------------------ navigation

    /// Skip by beats, preserving fractional beat position; moves an active loop too.
    pub fn skip_beats(&mut self, beats: f64) {
        let d = match self.frames_per_beat() {
            Some(fpb) => beats * fpb,
            None => beats * self.src_rate() * 0.5,
        };
        if self.loop_on {
            if let Some(mut l) = self.loop_region {
                l.start += d;
                l.end += d;
                self.loop_region = Some(l);
            }
        }
        self.jump_to(self.pos + d);
        if self.was_slip_engaged {
            self.slip_pos += d;
        }
    }

    // ------------------------------------------------------------ tempo

    pub fn set_tempo_fader(&mut self, v: f64) {
        self.tempo_fader = v.clamp(-1.0, 1.0);
    }

    pub fn tempo_fader(&self) -> f64 {
        self.tempo_fader
    }

    pub fn set_tempo_range(&mut self, r: TempoRange) {
        // Keep the current speed when the range changes.
        let s = self.tempo_range.speed(self.tempo_fader);
        self.tempo_range = r;
        self.tempo_fader = r.fader_for_speed(s);
    }

    pub fn tempo_range(&self) -> TempoRange {
        self.tempo_range
    }

    /// Set the fader so the deck plays at `bpm` (used when it owns the clock).
    pub fn set_bpm(&mut self, bpm: f64) {
        if let Some(g) = self.grid() {
            let speed = bpm / g.bpm / self.speed_mult;
            self.tempo_fader = self.tempo_range.fader_for_speed(speed);
        }
    }

    pub fn set_speed_mult(&mut self, m: f64) {
        self.speed_mult = m;
    }

    pub fn set_bpm_bend(&mut self, bpm: f64) {
        self.bpm_bend = bpm;
    }

    pub fn set_sync(&mut self, on: bool) {
        self.sync = on;
        if on {
            self.resync_requested = true;
        }
    }

    pub fn set_keylock(&mut self, on: bool) {
        self.keylock = on;
        self.xfade = XFADE_FRAMES;
    }

    pub fn set_semitones(&mut self, st: f64) {
        self.semitones = st.clamp(-24.0, 24.0);
    }

    pub fn semitones(&self) -> f64 {
        self.semitones
    }

    pub fn keylock(&self) -> bool {
        self.keylock
    }

    /// EARLY (-1) / LATE (+1) while held; 0 releases.
    pub fn phase_bend(&mut self, dir: f64) {
        self.phase_bend = dir.signum() * (dir != 0.0) as i32 as f64;
    }

    pub fn phase_reset(&mut self) {
        self.phase_offset = 0.0;
    }

    pub fn set_reverse(&mut self, on: bool) {
        if on && self.slip && !self.was_slip_engaged {
            self.slip_pos = self.pos;
        }
        self.reverse = on;
        self.xfade = XFADE_FRAMES;
    }

    pub fn backspin(&mut self, pressed: bool) {
        if pressed && self.playing {
            if self.slip && !self.was_slip_engaged {
                self.slip_pos = self.pos;
            }
            self.backspin = Some(-3.0);
            self.backspin_held = true;
        } else {
            self.backspin_held = false;
        }
    }

    pub fn set_slip(&mut self, on: bool) {
        if !on && self.was_slip_engaged {
            self.jump_to(self.slip_pos);
        }
        self.slip = on;
        self.slip_pos = self.pos;
    }

    pub fn set_quantize(&mut self, q: Quantize) {
        self.quantize = q;
    }

    /// Move the grid so that a downbeat sits at the current position.
    pub fn grid_start_here(&mut self) -> Option<BeatGrid> {
        let g = self.grid()?;
        let ng = BeatGrid { bpm: g.bpm, anchor_secs: self.position_secs() };
        self.replace_grid(Some(ng));
        Some(ng)
    }

    /// Replace the beat grid (the track itself is immutable, so this clones
    /// the metadata into a fresh Arc; audio data is shared).
    pub fn replace_grid(&mut self, grid: Option<BeatGrid>) {
        // The Track is shared and immutable; keep an override instead.
        self.grid_override = grid;
    }

    // ------------------------------------------------------------ render

    fn slip_engaged(&self) -> bool {
        self.roll.is_some()
            || (self.slip
                && (self.loop_on || self.reverse || self.backspin.is_some() || (self.momentary_cues && self.held_cue.is_some())))
    }

    /// Render one block. `out` is overwritten.
    pub fn process(&mut self, out: &mut [Frame], ctx: &DeckCtx) {
        let Some(track) = self.track.clone() else {
            out.fill([0.0; 2]);
            return;
        };
        let frames = &track.audio.frames[..];
        let stems = self.stems.clone();
        let stem_coeff = (1.0 - (-1.0 / (STEM_SMOOTH_SECS * self.out_rate)).exp()) as f32;
        let len = frames.len() as f64;
        let sr = track.audio.sample_rate as f64;
        let grid = self.grid();
        let n = out.len();

        // ---- speed & phase correction (once per block)
        let mut speed = self.base_speed(ctx);
        let follower = self.sync && !ctx.is_clock_owner;
        self.phase_offset += self.phase_bend * 0.5 * n as f64 / self.out_rate; // 0.5 beat/s
        if !follower && self.phase_bend != 0.0 {
            speed *= 1.0 + 0.04 * self.phase_bend;
        }
        self.last_phase_error = 0.0;
        if let (true, Some(g), true) = (follower, grid, self.playing && self.motor >= 1.0) {
            let deck_beat = g.beat_at(self.pos / sr);
            let target = ctx.clock_beat * self.speed_mult + self.phase_offset;
            let mut e = deck_beat - target;
            e -= e.round();
            self.last_phase_error = e;
            if self.resync_requested || e.abs() > 0.25 {
                // Snap into phase.
                self.pos -= e * g.seconds_per_beat() * sr;
                self.stretcher.reset();
                self.xfade = XFADE_FRAMES;
                self.resync_requested = false;
            } else {
                let tau = 0.35; // seconds to converge
                let corr = (-e * 60.0 / (g.bpm * tau)).clamp(-0.04, 0.04);
                speed *= 1.0 + corr;
            }
        } else if self.resync_requested && !self.playing {
            // Keep the flag until we start playing.
        } else {
            self.resync_requested = false;
        }
        self.last_speed = speed;

        let motor_step = 1.0 / (ctx.tape_stop_ms.max(0.0) * 0.001 * self.out_rate).max(1.0);
        let backspin_decay = {
            // Decay to ~0 over 4 beats.
            let bps = grid.map(|g| g.bpm * speed / 60.0).unwrap_or(2.0);
            (-(bps / 4.0) * 5.0 / self.out_rate).exp()
        };
        let semis_ratio = 2f64.powf(self.semitones / 12.0);
        let cbpf = ctx.clock_beats_per_frame();
        let fpb = grid.map(|g| g.seconds_per_beat() * sr);

        for (i, o) in out.iter_mut().enumerate() {
            // ---- quantized pending actions
            if let Some(p) = self.pending {
                let now = if p.on_clock {
                    Some(ctx.clock_beat + i as f64 * cbpf)
                } else {
                    grid.map(|g| g.beat_at(self.pos / sr))
                };
                if now.map(|b| b >= p.at_beat).unwrap_or(true) {
                    self.pending = None;
                    match p.action {
                        PendingAction::Play => self.start_now(ctx),
                        PendingAction::JumpCue(c) => self.fire_cue(c, false, ctx),
                        PendingAction::CueLoop(c) => self.fire_cue(c, true, ctx),
                    }
                }
            }

            // ---- motor (tape start/stop)
            if self.playing {
                self.motor = 1.0;
            } else if self.motor > 0.0 {
                self.motor = (self.motor - motor_step).max(0.0);
            }

            let mut v = speed * self.motor;
            if let Some(bs) = self.backspin.as_mut() {
                *bs *= backspin_decay;
                v = *bs * self.motor;
                if bs.abs() < 0.02 {
                    self.backspin = None;
                    if self.slip {
                        self.pos = self.slip_pos;
                        self.stretcher.reset();
                        self.xfade = XFADE_FRAMES;
                    } else {
                        self.playing = false;
                        self.motor = 0.0;
                        self.emit(DeckEvent::PlayState { playing: false });
                    }
                }
            } else if self.reverse {
                v = -v;
            }

            if self.motor <= 0.0 {
                *o = [0.0; 2];
                continue;
            }

            let stems_mix = stems.as_deref().and_then(|st| {
                for (g, t) in self.stem_gain.iter_mut().zip(self.stem_target) {
                    *g += (t - *g) * stem_coeff;
                    if (t - *g).abs() < 1e-5 {
                        *g = t;
                    }
                }
                // All stems at unity is exactly the original mix: skip the extra work.
                (self.stem_gain != [1.0; NUM_STEMS]).then_some((st, self.stem_gain))
            });
            let reader = Reader {
                frames,
                looping: if self.loop_on { self.loop_region } else { None },
                stems: stems_mix,
            };

            let want_stretch =
                (self.keylock || self.semitones != 0.0) && self.motor >= 1.0 && v > 0.0 && self.backspin.is_none();
            if want_stretch != self.stretch_active {
                self.stretch_active = want_stretch;
                self.stretcher.reset();
                self.xfade = XFADE_FRAMES;
            }

            let vari = reader.at(self.pos);
            let sample = if self.stretch_active {
                let read_rate = self.step * if self.keylock { 1.0 } else { v } * semis_ratio;
                let s = self.stretcher.tick(&reader, self.pos, read_rate);
                if self.xfade > 0 {
                    let t = self.xfade as f32 / XFADE_FRAMES as f32;
                    self.xfade -= 1;
                    [s[0] * (1.0 - t) + vari[0] * t, s[1] * (1.0 - t) + vari[1] * t]
                } else {
                    s
                }
            } else {
                if self.xfade > 0 {
                    self.xfade -= 1;
                }
                vari
            };
            // Tape stop also lowers pitch naturally via varispeed (motor < 1).
            *o = sample;

            // ---- advance
            let d = self.step * v;
            self.pos += d;
            let engaged = self.slip_engaged();
            if engaged {
                if !self.was_slip_engaged {
                    // slip_pos was captured at engagement time by callers;
                    // if not, start from here.
                }
                self.slip_pos += self.step * speed.abs();
            } else if self.was_slip_engaged && self.roll.is_none() {
                // Disengaged: return to where we would have been.
                self.pos = self.slip_pos;
                self.stretcher.reset();
                self.xfade = XFADE_FRAMES;
            } else {
                self.slip_pos = self.pos;
            }
            self.was_slip_engaged = engaged;

            if self.loop_on {
                if let Some(l) = self.loop_region {
                    let ll = l.len().max(1.0);
                    if d > 0.0 && self.pos >= l.end && self.pos < l.end + ll.max(fpb.unwrap_or(0.0)) * 4.0 {
                        self.pos -= ll * ((self.pos - l.start) / ll).floor();
                    } else if d < 0.0 && self.pos < l.start && self.pos > l.start - ll {
                        self.pos += ll;
                    }
                }
            }
            if self.pos >= len {
                self.pos = len;
                if self.playing {
                    self.playing = false;
                    self.motor = 0.0;
                    self.emit(DeckEvent::PlayState { playing: false });
                    self.emit(DeckEvent::EndOfTrack);
                }
            } else if self.pos < 0.0 {
                self.pos = 0.0;
            }
        }
    }
}
