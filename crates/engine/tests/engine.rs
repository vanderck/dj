use std::sync::Arc;

use dj_engine::deck::Quantize;
use dj_engine::dsp::Frame;
use dj_engine::engine::{ClockOwner, DeckCmd, NUM_DECKS};
use dj_engine::fx::{Effect, FxCtx, InsertPos};
use dj_engine::mixer::ChannelParams;
use dj_engine::track::{AudioData, BeatGrid, Track};
use dj_engine::{create, Command, Controller, Engine, EngineConfig, Event, Garbage};

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
impl SnapExt for dj_engine::Snapshot {
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
        Event::Deck { event: dj_engine::deck::DeckEvent::CueSet { idx: 2, secs, .. }, .. } => Some(*secs),
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
fn cue_beat_loop_loops_from_the_cue() {
    let (mut e, mut c) = setup();
    send(&mut c, Command::GlobalQuantize(None));
    send(&mut c, Command::Load { deck: 0, track: click_track(1, 120.0, 60.0, 48000), start_at_first_cue: false });
    // A cue at 10 s, launched while stopped: plays, looping 4 beats (2 s at 120 BPM) from 10 s.
    deck(&mut c, 0, DeckCmd::SetCue { idx: 0, secs: Some(10.0), end_secs: None });
    deck(&mut c, 0, DeckCmd::CueBeatLoop { idx: 0, beats: 4.0 });
    run(&mut e, 2);
    let s = e.snapshot().decks[0];
    assert!(s.playing && s.loop_on, "playing in a loop");
    assert!((s.loop_start_secs - 10.0).abs() < 1e-6 && (s.loop_end_secs - 12.0).abs() < 1e-6, "{} .. {}", s.loop_start_secs, s.loop_end_secs);
    assert!((s.loop_beats - 4.0).abs() < 1e-6);
    // 3 s later the playhead is still inside the loop.
    run(&mut e, (3.0 * SR as f64 / BLOCK as f64) as usize);
    let pos = e.snapshot().decks[0].position_secs;
    assert!((10.0..12.0).contains(&pos), "looping: {pos}");
    // An empty pad does nothing.
    deck(&mut c, 0, DeckCmd::CueBeatLoop { idx: 5, beats: 4.0 });
    run(&mut e, 1);
    assert!((e.snapshot().decks[0].loop_start_secs - 10.0).abs() < 1e-6);
}

#[test]
fn release_loop_jumps_to_the_loop_start_on_the_beat() {
    let (mut e, mut c) = setup();
    send(&mut c, Command::GlobalQuantize(None)); // released on the beat anyway
    send(&mut c, Command::Load { deck: 0, track: click_track(1, 120.0, 60.0, 48000), start_at_first_cue: false });
    deck(&mut c, 0, DeckCmd::SetCue { idx: 0, secs: Some(10.0), end_secs: None });
    deck(&mut c, 0, DeckCmd::CueBeatLoop { idx: 0, beats: 4.0 });
    run(&mut e, (1.3 * SR as f64 / BLOCK as f64) as usize); // inside the loop, mid-beat
    deck(&mut c, 0, DeckCmd::ReleaseLoop { to_cue: None, to_secs: None });
    run(&mut e, 2);
    assert!(e.snapshot().decks[0].loop_on, "waits for the beat");
    run(&mut e, (0.6 * SR as f64 / BLOCK as f64) as usize);
    let s = e.snapshot().decks[0];
    assert!(!s.loop_on, "loop released");
    // Released on the beat at 1.5 s into the loop: back to 10 s, then ~0.4 s further.
    assert!((10.0..10.6).contains(&s.position_secs), "{}", s.position_secs);
    run(&mut e, (3.0 * SR as f64 / BLOCK as f64) as usize);
    assert!(e.snapshot().decks[0].position_secs > 12.5, "plays on past the old loop end");
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
            effect: dj_engine::fx::native::NativeFx::Echo.create(SR),
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

// ------------------------------------------------------------------ stems

use dj_engine::track::{Stem, StemData};

/// Window that holds whole periods of both test tones (12 x 60 Hz, 200 x 1 kHz at 48 kHz).
const TONE_WIN: usize = 9600;

/// Energy of `hz` in the first `TONE_WIN` frames of `x` (left channel), via Goertzel.
fn tone_energy(x: &[Frame], hz: f32, sr: f32) -> f32 {
    let x = &x[..TONE_WIN];
    let w = 2.0 * std::f32::consts::PI * hz / sr;
    let (mut s1, mut s2) = (0.0f32, 0.0f32);
    for f in x {
        let s0 = f[0] + 2.0 * w.cos() * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    (s1 * s1 + s2 * s2 - 2.0 * w.cos() * s1 * s2) / x.len() as f32
}

/// A track that is a 60 Hz "bass" plus a 1 kHz "vocal", with matching stems.
fn stem_track(id: u64) -> (Arc<Track>, Arc<StemData>) {
    let n = 10 * SR as usize;
    let sine = |hz: f32, a: f32| -> Vec<Frame> {
        (0..n).map(|i| {
            let v = a * (2.0 * std::f32::consts::PI * hz * i as f32 / SR).sin();
            [v, v]
        }).collect()
    };
    let (bass, vocals, drums) = (sine(60.0, 0.4), sine(1000.0, 0.3), vec![[0.0; 2]; n]);
    let frames = bass.iter().zip(&vocals).map(|(b, v)| [b[0] + v[0], b[1] + v[1]]).collect();
    let track = Arc::new(Track {
        id,
        audio: AudioData { sample_rate: SR as u32, frames },
        grid: None,
        gain: 1.0,
        cues: Default::default(),
    });
    (track, Arc::new(StemData::from_f32(id, &drums, &bass, &vocals)))
}

fn stem_garbage(events: &[Event]) -> Vec<u64> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Garbage(Garbage::Stems(s)) => Some(s.track_id),
            _ => None,
        })
        .collect()
}

#[test]
fn stems_mute_removes_the_vocal_and_dispose_off_thread() {
    let (mut e, mut c) = setup();
    let (track, stems) = stem_track(42);
    send(&mut c, Command::Load { deck: 0, track, start_at_first_cue: false });
    send(&mut c, Command::GlobalQuantize(None));
    deck(&mut c, 0, DeckCmd::KeyLock(false));
    deck(&mut c, 0, DeckCmd::Play);
    // Muting before stems exist has no audible effect yet.
    deck(&mut c, 0, DeckCmd::StemGain { stem: Stem::Vocals as u8, gain: 0.0 });
    let before = run(&mut e, 60);
    let vocal_before = tone_energy(&before[4096..], 1000.0, SR);
    let bass_before = tone_energy(&before[4096..], 60.0, SR);
    assert!(vocal_before > 0.1 * bass_before, "vocal audible before stems attach");
    assert!(!e.snapshot().decks[0].stems_ready);

    // Stems for a different track are rejected and handed back.
    let (_, wrong) = stem_track(7);
    send(&mut c, Command::AttachStems { deck: 0, stems: wrong });
    send(&mut c, Command::AttachStems { deck: 0, stems });
    run(&mut e, 4); // let the 4 ms gain ramp settle
    assert_eq!(stem_garbage(&drain(&mut c)), vec![7]);
    let snap = e.snapshot();
    assert!(snap.decks[0].stems_ready);
    assert_eq!(snap.decks[0].stem_gains[Stem::Vocals as usize], 0.0);

    let after = run(&mut e, 40);
    let vocal_after = tone_energy(&after, 1000.0, SR);
    let bass_after = tone_energy(&after, 60.0, SR);
    assert!(vocal_after < 1e-4 * vocal_before, "vocal removed: {vocal_after} vs {vocal_before}");
    assert!((bass_after / bass_before - 1.0).abs() < 0.05, "bass untouched");

    // Unmuting brings it back.
    deck(&mut c, 0, DeckCmd::StemGain { stem: Stem::Vocals as u8, gain: 1.0 });
    run(&mut e, 4);
    let back = run(&mut e, 40);
    assert!((tone_energy(&back, 1000.0, SR) / vocal_before - 1.0).abs() < 0.05);

    // Loading another track drops the stems (returned for disposal) and resets gains.
    deck(&mut c, 0, DeckCmd::StemGain { stem: Stem::Drums as u8, gain: 0.0 });
    let (next, _) = stem_track(43);
    send(&mut c, Command::Load { deck: 0, track: next, start_at_first_cue: false });
    run(&mut e, 2);
    assert_eq!(stem_garbage(&drain(&mut c)), vec![42]);
    let snap = e.snapshot();
    assert!(!snap.decks[0].stems_ready);
    assert_eq!(snap.decks[0].stem_gains, [1.0; 4]);
}

#[test]
fn stems_survive_keylock_and_loops() {
    let (mut e, mut c) = setup();
    let (track, stems) = stem_track(5);
    send(&mut c, Command::Load { deck: 0, track, start_at_first_cue: false });
    send(&mut c, Command::AttachStems { deck: 0, stems });
    send(&mut c, Command::GlobalQuantize(None));
    deck(&mut c, 0, DeckCmd::KeyLock(true));
    deck(&mut c, 0, DeckCmd::Tempo(0.5)); // speed change => time-stretch path
    deck(&mut c, 0, DeckCmd::Play);
    deck(&mut c, 0, DeckCmd::StemGain { stem: Stem::Bass as u8, gain: 0.0 });
    let out = run(&mut e, 100);
    let tail = &out[out.len() - TONE_WIN..];
    let (bass, vocal) = (tone_energy(tail, 60.0, SR), tone_energy(tail, 1000.0, SR));
    assert!(vocal > 100.0 * bass, "bass muted through the stretcher: {bass} vs {vocal}");
}

// ------------------------------------------------------------------ timing

/// When a click placed at exactly 1.000 s of the track is loudest in the output, relative to when
/// the deck reaches 1.000 s (in ms; positive = late).
fn click_lag_ms(keylock: bool, tempo: f64) -> f64 {
    let sr = SR as u32;
    let mut frames = vec![[0.0f32; 2]; sr as usize * 4];
    for f in frames.iter_mut().skip(sr as usize).take(32) {
        *f = [0.9, 0.9];
    }
    let track = Arc::new(Track { id: 1, audio: AudioData { sample_rate: sr, frames }, grid: None, gain: 1.0, cues: Default::default() });
    let (mut e, mut c) = setup();
    send(&mut c, Command::Load { deck: 0, track, start_at_first_cue: false });
    send(&mut c, Command::GlobalQuantize(None));
    deck(&mut c, 0, DeckCmd::KeyLock(keylock));
    deck(&mut c, 0, DeckCmd::Tempo(tempo));
    deck(&mut c, 0, DeckCmd::Play);
    let out = run(&mut e, sr as usize * 2 / BLOCK);
    let speed = e.snapshot().decks[0].speed;
    let peak = out.iter().enumerate().max_by(|a, b| a.1[0].abs().total_cmp(&b.1[0].abs())).unwrap().0;
    peak as f64 * 1000.0 / SR as f64 - 1000.0 / speed
}

#[test]
fn key_lock_does_not_delay_audio_against_the_playhead() {
    // Cue markers and the waveform are drawn at the deck position; what you hear must match.
    // (The stretcher used to start each grain a hop early: everything was ~33 ms late.)
    let plain = click_lag_ms(false, 0.0);
    for tempo in [0.0, 0.3, -0.3, 0.8, -0.8] {
        let locked = click_lag_ms(true, tempo);
        assert!((locked - plain).abs() < 6.0, "key lock at tempo {tempo}: {locked:+.1} ms vs {plain:+.1} ms without");
    }
}

// ------------------------------------------------------------------ direct outputs

#[test]
fn decks_routed_to_direct_pairs_reach_only_their_own_outputs() {
    use dj_engine::mixer::{ExtStages, Output};
    let (mut e, mut c) = setup();
    let (tones, _) = stem_track(61); // deck 1: 60 Hz + 1 kHz
    send(&mut c, Command::Load { deck: 0, track: tones, start_at_first_cue: false });
    send(&mut c, Command::Load { deck: 1, track: click_track(62, 120.0, 10.0, SR as u32), start_at_first_cue: false });
    send(&mut c, Command::GlobalQuantize(None));
    // External mixer: no internal stages, each channel to its own stereo pair.
    send(&mut c, Command::ExtStages(ExtStages { trim: false, eq: false, filter: false, volume: false }));
    for ch in 0..2u8 {
        send(&mut c, Command::Channel { ch, params: ChannelParams { output: Output::Pair(ch), volume: 0.0, ..Default::default() } });
        deck(&mut c, ch, DeckCmd::Play);
    }
    let energy = |x: &[Frame]| x.iter().map(|f| f[0] * f[0]).sum::<f32>();
    let (mut master, mut p0, mut p1) = (0.0, Vec::new(), Vec::new());
    for _ in 0..80 {
        e.process(BLOCK);
        master += energy(e.master(BLOCK));
        p0.extend_from_slice(e.pair(0, BLOCK).expect("pair 0 in use"));
        p1.extend_from_slice(e.pair(1, BLOCK).expect("pair 1 in use"));
    }
    assert!(master < 1e-6, "nothing on the master bus: {master}");
    // Channel volume is at 0 but bypassed, so both decks are heard at full level.
    assert!(tone_energy(&p0[4096..], 1000.0, SR) > 1e-3, "deck 1 tone on outputs 1/2");
    assert!(tone_energy(&p1[4096..], 1000.0, SR) < 1e-4 * tone_energy(&p0[4096..], 1000.0, SR), "deck 1 not on 3/4");
    assert!(p1.iter().any(|f| f[0] > 0.5), "deck 2 clicks on outputs 3/4");
}

#[test]
fn empty_decks_do_not_loop_and_gridless_loops_report_their_length() {
    let (mut e, mut c) = setup();
    send(&mut c, Command::GlobalQuantize(None));
    // Nothing loaded: every way into a loop is ignored.
    deck(&mut c, 2, DeckCmd::LoopBeats(4.0));
    deck(&mut c, 2, DeckCmd::LoopToggle);
    deck(&mut c, 2, DeckCmd::LoopIn);
    deck(&mut c, 2, DeckCmd::LoopOut);
    deck(&mut c, 2, DeckCmd::FreeLoop(50.0));
    deck(&mut c, 2, DeckCmd::SlipLoop { beats: 1.0, pressed: true });
    run(&mut e, 2);
    assert!(!e.snapshot().decks[2].loop_on, "an empty deck never loops");

    // A track without a grid loops at an assumed 120 BPM, and says how many beats.
    let t = click_track(1, 120.0, 30.0, 48000);
    let audio = AudioData { sample_rate: t.audio.sample_rate, frames: t.audio.frames.to_vec() };
    let gridless = Track { id: 2, audio, grid: None, gain: 1.0, cues: Default::default() };
    send(&mut c, Command::Load { deck: 0, track: Arc::new(gridless), start_at_first_cue: false });
    deck(&mut c, 0, DeckCmd::LoopBeats(4.0));
    run(&mut e, 2);
    let s = e.snapshot().decks[0];
    assert!(s.loop_on && !s.has_grid, "loop {} grid {}", s.loop_on, s.has_grid);
    assert!((s.loop_beats - 4.0).abs() < 1e-9, "{}", s.loop_beats);
}

#[test]
fn a_playing_deck_takes_the_clock_when_nobody_has_it() {
    let (mut e, mut c) = setup();
    send(&mut c, Command::GlobalQuantize(None));
    send(&mut c, Command::Load { deck: 0, track: click_track(1, 126.0, 60.0, 48000), start_at_first_cue: false });
    deck(&mut c, 0, DeckCmd::Play);
    run(&mut e, 10);
    assert_eq!(e.snapshot().clock.owner, ClockOwner::Deck(0));
    // The master is ejected while nothing else plays: no deck can have the clock.
    send(&mut c, Command::Eject { deck: 0 });
    run(&mut e, 4);
    assert_eq!(e.snapshot().clock.owner, ClockOwner::Header);
    // The next deck that plays becomes master, at its own tempo (not synced to a hidden one).
    send(&mut c, Command::Load { deck: 1, track: click_track(2, 122.0, 60.0, 48000), start_at_first_cue: false });
    deck(&mut c, 1, DeckCmd::Sync(false));
    deck(&mut c, 1, DeckCmd::Play);
    run(&mut e, 10);
    let s = e.snapshot();
    assert_eq!(s.clock.owner, ClockOwner::Deck(1));
    assert!((s.clock.bpm - 122.0).abs() < 0.01, "{}", s.clock.bpm);
    assert!((s.decks[1].bpm - 122.0).abs() < 0.01, "kept its tempo: {}", s.decks[1].bpm);
}
