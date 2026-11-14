use std::sync::Arc;

use collective_engine::deck::Quantize;
use collective_engine::dsp::Frame;
use collective_engine::engine::{ClockOwner, DeckCmd, NUM_DECKS};
use collective_engine::fx::{Effect, FxCtx, InsertPos};
use collective_engine::mixer::ChannelParams;
use collective_engine::track::{AudioData, BeatGrid, Track};
use collective_engine::{create, Command, Controller, Engine, EngineConfig, Event, Garbage};

const SR: f32 = 48000.0;
const BLOCK: usize = 256;

/// A click track: a short burst on every beat.
fn click_track(id: u64, bpm: f64, secs: f64, sr: u32) -> Arc<Track> {
    let n = (secs * sr as f64) as usize;
    let fpb = 60.0 / bpm * sr as f64;
    let mut frames = vec![[0.0f32; 2]; n];
    let mut b = 0.0;
    while (b * fpb) < n as f64 {
        let s = (b * fpb) as usize;
        for f in frames.iter_mut().skip(s).take(64) {
            *f = [0.8, 0.8];
        }
        b += 1.0;
    }
    Arc::new(Track {
        id,
        audio: AudioData { sample_rate: sr, frames },
        grid: Some(BeatGrid { bpm, anchor_secs: 0.0 }),
        gain: 1.0,
        cues: Default::default(),
    })
}

fn setup() -> (Engine, Controller) {
    create(EngineConfig { sample_rate: SR, max_block: 1024, max_latency: 4096 })
}

fn run(e: &mut Engine, blocks: usize) -> Vec<Frame> {
    let mut out = Vec::with_capacity(blocks * BLOCK);
    for _ in 0..blocks {
        e.process(BLOCK);
        out.extend_from_slice(e.master(BLOCK));
    }
    out
}

fn send(c: &mut Controller, cmd: Command) {
    assert!(c.send(cmd).is_ok());
}

fn deck(c: &mut Controller, d: u8, cmd: DeckCmd) {
    send(c, Command::Deck { deck: d, cmd });
}

fn drain(c: &mut Controller) -> Vec<Event> {
    let mut v = Vec::new();
    while let Some(e) = c.poll_event() {
        v.push(e);
    }
    v
}

#[test]
fn first_loaded_deck_becomes_master_and_plays() {
    let (mut e, mut c) = setup();
    send(&mut c, Command::Load { deck: 0, track: click_track(1, 124.0, 20.0, 44100), start_at_first_cue: false });
    send(&mut c, Command::GlobalQuantize(None));
    deck(&mut c, 0, DeckCmd::Play);
    let out = run(&mut e, 200);
    let snap = e.snapshot();
    assert_eq!(snap.clock.owner, ClockOwner::Deck(0));
    assert!((snap.clock.bpm - 124.0).abs() < 1e-6);
    assert!(snap.decks[0].playing);
    assert!(out.iter().any(|f| f[0] > 0.5), "audio reached the master bus");
}

#[test]
fn synced_deck_locks_tempo_and_phase() {
    let (mut e, mut c) = setup();
    send(&mut c, Command::GlobalQuantize(None));
    send(&mut c, Command::Load { deck: 0, track: click_track(1, 128.0, 60.0, 48000), start_at_first_cue: false });
    send(&mut c, Command::Load { deck: 1, track: click_track(2, 120.0, 60.0, 44100), start_at_first_cue: false });
    deck(&mut c, 0, DeckCmd::Play);
    run(&mut e, 50);
    // Start deck 2 off-phase: seek a bit, then play with sync.
    deck(&mut c, 1, DeckCmd::SeekFraction(0.0123));
    deck(&mut c, 1, DeckCmd::Sync(true));
    deck(&mut c, 1, DeckCmd::Play);
    run(&mut e, 400);
    let s = e.snapshot();
    assert!(s.decks[1].sync);
    assert!((s.decks[1].bpm - 128.0).abs() < 0.01, "bpm {}", s.decks[1].bpm);
    let frac = |b: f64| b - b.round();
    let err = frac(s.decks[1].beat - s.decks[0].beat);
    assert!(err.abs() < 0.01, "phase error {err}");
}

#[test]
fn quantized_play_waits_for_the_beat() {
    let (mut e, mut c) = setup();
    send(&mut c, Command::Load { deck: 0, track: click_track(1, 120.0, 60.0, 48000), start_at_first_cue: false });
    send(&mut c, Command::Load { deck: 1, track: click_track(2, 120.0, 60.0, 48000), start_at_first_cue: false });
    send(&mut c, Command::GlobalQuantize(None));
    deck(&mut c, 0, DeckCmd::Play);
    run(&mut e, 10);
    send(&mut c, Command::GlobalQuantize(Some(4.0)));
    deck(&mut c, 1, DeckCmd::Play);
    run(&mut e, 1);
    let s = e.snapshot();
    assert!(!s.decks[1].playing);
    // 1 bar at 120 BPM = 2 s.
    run(&mut e, (2.0 * SR as f64 / BLOCK as f64) as usize + 2);
    let s = e.snapshot();
    assert!(s.decks[1].playing);
    let err = s.decks[1].beat - s.decks[0].beat;
    assert!((err - err.round()).abs() < 0.01);
}

#[test]
fn loops_and_slip_roll() {
    let (mut e, mut c) = setup();
    send(&mut c, Command::GlobalQuantize(None));
    send(&mut c, Command::Load { deck: 0, track: click_track(1, 120.0, 60.0, 48000), start_at_first_cue: false });
    deck(&mut c, 0, DeckCmd::Play);
    run(&mut e, 100);
    deck(&mut c, 0, DeckCmd::LoopBeats(1.0));
    run(&mut e, 600); // ~3.2 s ≫ 1 beat (0.5 s)
    let s = e.snapshot();
    assert!(s.decks[0].loop_on);
    assert!((s.decks[0].loop_beats - 1.0).abs() < 1e-6);
    assert!(s.position_in_loop(0));
    deck(&mut c, 0, DeckCmd::LoopExit);
    run(&mut e, 1);

    // Slip roll: playhead continues underneath.
    let before = e.snapshot().decks[0].position_secs;
    deck(&mut c, 0, DeckCmd::SlipLoop { beats: 0.25, pressed: true });
    run(&mut e, 375); // 2 s
    deck(&mut c, 0, DeckCmd::SlipLoop { beats: 0.25, pressed: false });
    run(&mut e, 1);
    let after = e.snapshot().decks[0].position_secs;
    assert!((after - before - 2.0).abs() < 0.05, "slip returned to {after} (before {before})");
}

trait SnapExt {
    fn position_in_loop(&self, d: usize) -> bool;
}
impl SnapExt for collective_engine::Snapshot {
    fn position_in_loop(&self, d: usize) -> bool {
        let v = self.decks[d];
        v.position_secs >= v.loop_start_secs - 0.01 && v.position_secs <= v.loop_end_secs + 0.01
    }
}

#[test]
fn hot_cue_set_and_quantized_jump() {
    let (mut e, mut c) = setup();
    send(&mut c, Command::GlobalQuantize(None));
    send(&mut c, Command::Load { deck: 0, track: click_track(1, 120.0, 60.0, 48000), start_at_first_cue: false });
    deck(&mut c, 0, DeckCmd::Quantize(Quantize::Beats(1.0)));
    deck(&mut c, 0, DeckCmd::Play);
    run(&mut e, 400);
    deck(&mut c, 0, DeckCmd::HotCue { idx: 2, pressed: true });
    deck(&mut c, 0, DeckCmd::HotCue { idx: 2, pressed: false });
    run(&mut e, 1);
    let evs = drain(&mut c);
    let set = evs.iter().find_map(|e| match e {
        Event::Deck { event: collective_engine::deck::DeckEvent::CueSet { idx: 2, secs, .. }, .. } => Some(*secs),
        _ => None,
    });
    let secs = set.expect("cue set event");
    assert!((secs * 2.0 - (secs * 2.0).round()).abs() < 1e-6, "cue snapped to the beat: {secs}");
    run(&mut e, 300);
    deck(&mut c, 0, DeckCmd::HotCue { idx: 2, pressed: true });
    run(&mut e, 120); // ≤ 1 beat to fire + a bit
    let pos = e.snapshot().decks[0].position_secs;
    assert!(pos > secs && pos < secs + 0.5, "jumped to cue: {pos} vs {secs}");
}

#[test]
fn master_handoff_when_master_stops() {
    let (mut e, mut c) = setup();
    send(&mut c, Command::GlobalQuantize(None));
    send(&mut c, Command::Load { deck: 0, track: click_track(1, 126.0, 60.0, 48000), start_at_first_cue: false });
    send(&mut c, Command::Load { deck: 2, track: click_track(2, 122.0, 60.0, 48000), start_at_first_cue: false });
    deck(&mut c, 0, DeckCmd::Play);
    deck(&mut c, 2, DeckCmd::Play);
    run(&mut e, 20);
    assert_eq!(e.snapshot().clock.owner, ClockOwner::Deck(0));
    deck(&mut c, 0, DeckCmd::Pause);
    run(&mut e, 20);
    let s = e.snapshot();
    assert_eq!(s.clock.owner, ClockOwner::Deck(2));
    assert!((s.clock.bpm - 126.0).abs() < 0.01, "tempo carried over: {}", s.clock.bpm);
}

#[test]
fn wallclock_keeps_header_tempo() {
    let (mut e, mut c) = setup();
    send(&mut c, Command::Wallclock(true));
    send(&mut c, Command::SetBpm(100.0));
    send(&mut c, Command::Load { deck: 0, track: click_track(1, 126.0, 60.0, 48000), start_at_first_cue: false });
    deck(&mut c, 0, DeckCmd::Sync(true));
    deck(&mut c, 0, DeckCmd::Play);
    run(&mut e, 100);
    let s = e.snapshot();
    assert_eq!(s.clock.owner, ClockOwner::Header);
    assert!((s.decks[0].bpm - 100.0).abs() < 0.01);
}

struct Latent {
    buf: Vec<Frame>,
    pos: usize,
}
impl Effect for Latent {
    fn set_param(&mut self, _: usize, _: f32) {}
    fn process(&mut self, b: &mut [Frame], _: &FxCtx) {
        for f in b {
            let o = self.buf[self.pos];
            self.buf[self.pos] = *f;
            self.pos = (self.pos + 1) % self.buf.len();
            *f = o;
        }
    }
    fn latency(&self) -> usize {
        self.buf.len()
    }
}

#[test]
fn plugin_latency_is_compensated_across_channels() {
    let (mut e, mut c) = setup();
    send(&mut c, Command::GlobalQuantize(None));
    send(&mut c, Command::Wallclock(true));
    for d in 0..2u8 {
        send(&mut c, Command::Load { deck: d, track: click_track(d as u64 + 1, 120.0, 30.0, 48000), start_at_first_cue: false });
        deck(&mut c, d, DeckCmd::Sync(false));
    }
    send(
        &mut c,
        Command::FxInsert {
            ch: 0,
            slot: 0,
            effect: Box::new(Latent { buf: vec![[0.0; 2]; 300], pos: 0 }),
            pos: InsertPos::Pre,
            on: true,
            mix: 1.0,
        },
    );
    // Channel 2 only: listen to deck 2 alone and find its first click.
    send(&mut c, Command::Channel { ch: 0, params: ChannelParams { mute: true, ..Default::default() } });
    deck(&mut c, 1, DeckCmd::Play);
    let out = run(&mut e, 20);
    let first = out.iter().position(|f| f[0] > 0.5).unwrap();
    assert_eq!(e.snapshot().compensation_latency, 300);
    let expect = e.output_latency() as i64; // 300 (plugin) + limiter look-ahead
    assert!(expect > 300);
    assert!((first as i64 - expect).abs() <= 1, "deck 2 delayed by the compensation: {first} vs {expect}");
}

#[test]
fn removed_effects_are_returned_for_disposal() {
    let (mut e, mut c) = setup();
    send(
        &mut c,
        Command::FxInsert {
            ch: 3,
            slot: 1,
            effect: collective_engine::fx::native::NativeFx::Echo.create(SR),
            pos: InsertPos::Post,
            on: true,
            mix: 1.0,
        },
    );
    send(&mut c, Command::FxRemove { ch: 3, slot: 1 });
    run(&mut e, 1);
    let evs = drain(&mut c);
    assert!(evs.iter().any(|e| matches!(e, Event::Garbage(Garbage::Fx(_)))));
    assert!(NUM_DECKS == 4);
}
