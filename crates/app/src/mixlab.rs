//! Offline mix lab (test only): load two real tracks, let dj plan the mix, render it through the
//! engine to a WAV, and dump the analysis behind it. Driven by environment variables:
//!
//! MIX_A, MIX_B (outgoing, incoming), MIX_OUT (folder), MIX_DEMUCS, MIX_PREPARED, MIX_MODE
//! (best|now), and optionally MIX_FORCE ("out pad,in pad,bars": a chosen transition). Run with `cargo test -p dj mixlab -- --ignored --nocapture`.

use super::automix::MixWhen;
use super::*;
use crate::settings::MixStyle;
use crate::transition::{self as tr, Bars, Candidate, Limits, Technique};
use dj_engine::engine::{create, EngineConfig};
use serde_json::{json, Value};

const SR: f64 = 48000.0;

fn env(k: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| panic!("set {k}"))
}

fn pump(e: &mut dj_engine::Engine, c: &mut Core, rec: Option<&mut Vec<[f32; 2]>>) {
    let mut rec = rec;
    for _ in 0..4 {
        e.process(512);
        if let Some(r) = rec.as_deref_mut() {
            r.extend_from_slice(e.master(512));
        }
    }
    c.tick();
}

fn write_wav(path: &std::path::Path, frames: &[[f32; 2]]) {
    let mut out = Vec::with_capacity(44 + frames.len() * 4);
    let data_len = (frames.len() * 4) as u32;
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&(SR as u32).to_le_bytes());
    out.extend_from_slice(&(SR as u32 * 4).to_le_bytes());
    out.extend_from_slice(&4u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for f in frames {
        for s in f {
            out.extend_from_slice(&((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
        }
    }
    std::fs::write(path, out).unwrap();
}

/// A track's analysis, bar by bar, as dj sees it.
fn bars_json(b: &Bars, cues: &[(usize, String, f64)]) -> Value {
    let stems = b.stems.as_ref().map(|s| s.iter().map(|v| v.iter().map(|x| (x * 1000.0).round() / 1000.0).collect::<Vec<_>>()).collect::<Vec<_>>());
    json!({
        "start": b.start, "bar_secs": b.bar_secs, "bars": b.len(),
        "drops": b.drops(), "intro_end": b.intro_end(), "outro_start": b.outro_start(),
        "loud": b.loud.iter().map(|x| (x * 1000.0).round() / 1000.0).collect::<Vec<_>>(),
        "stems": stems,
        "cues": cues.iter().map(|(i, n, s)| json!({"pad": i + 1, "name": n, "secs": s, "bar": (s - b.start) / b.bar_secs})).collect::<Vec<_>>(),
    })
}

fn plan_json(p: &automix::MixPlan) -> Value {
    let blend = p.blend.as_ref().map(|b| json!({"bars": b.bars, "swap": b.swap, "in_at": b.in_at, "in_ramp": b.in_ramp, "out_at": b.out_at, "out_ramp": b.out_ramp, "notes": b.notes}));
    json!({
        "how": format!("{:?}", p.how), "mode": format!("{:?}", p.mode), "start_beat": p.start_beat, "length_beats": p.length_beats,
        "lead_in": p.lead_in, "entry_secs": p.entry_secs, "filter": p.filter, "echo_at": p.echo_at,
        "reason": p.reason, "why_how": p.why_how, "notes": p.notes, "blend": blend,
    })
}

#[test]
#[ignore]
fn mixlab() {
    let (a, b, out) = (env("MIX_A"), env("MIX_B"), PathBuf::from(env("MIX_OUT")));
    let mode = if std::env::var("MIX_MODE").as_deref() == Ok("now") { MixWhen::Now } else { MixWhen::Best };
    std::fs::create_dir_all(&out).unwrap();
    let cfg = out.join("cfg");
    let _ = std::fs::remove_dir_all(&cfg);
    std::env::set_var("DJ_CONFIG_DIR", &cfg);
    let (mut e, ctl) = create(EngineConfig { sample_rate: SR as f32, max_block: 512, max_latency: 4096 });
    let settings = Settings {
        osc_enabled: false,
        vst3_enabled: false,
        library_folders: vec![],
        stems_demucs: Some(PathBuf::from(env("MIX_DEMUCS"))),
        stems_prepared_dir: Some(PathBuf::from(env("MIX_PREPARED"))),
        ..Default::default()
    };
    let mut c = Core::new(ctl, SR as f32, 512, settings, Session::default());
    c.sync_engine();
    for (d, p) in [(0, &a), (1, &b)] {
        c.load_track(d, std::path::Path::new(p));
    }
    let t0 = Instant::now();
    let pending = |c: &Core, d: usize| c.decks[d].info.as_ref().is_none_or(|i| i.cues_pending);
    while !(0..2).all(|d| c.snap.decks[d].loaded && c.decks[d].info.is_some() && c.snap.decks[d].stems_ready && c.decks[d].stem_wave.is_some() && !pending(&c, d)) {
        assert!(t0.elapsed().as_secs() < 900, "decks never ready");
        pump(&mut e, &mut c, None);
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    // dj's deck-order tip (nothing is playing yet).
    eprintln!("order tip: {:?}", c.mix_order_hint().map(|(a, b)| (a + 1, b + 1)));
    c.route("Mixer/CH1/Volume", &[OscType::Float(1.0)]);
    c.route("Mixer/CH2/Volume", &[OscType::Float(0.0)]);
    c.deck_cmd(0, DeckCmd::Play);
    for _ in 0..20 {
        pump(&mut e, &mut c, None);
    }
    assert_eq!(c.mix_target(1), Some(0), "deck 2 should be mixable into deck 1");

    // ---- The analysis dj works from.
    let (vi, vo) = (c.snap.decks[1], c.snap.decks[0]);
    let (spb_i, spb_o) = (60.0 / vi.track_bpm, 60.0 / vo.track_bpm);
    let (start_i, start_o) = (vi.position_secs - vi.beat * spb_i, vo.position_secs - vo.beat * spb_o);
    let (ii, oi) = (c.decks[1].info.as_ref().unwrap(), c.decks[0].info.as_ref().unwrap());
    let inc = Bars::new(&ii.waveform, c.decks[1].stem_wave.as_deref(), start_i, spb_i, vi.duration_secs);
    let outb = Bars::new(&oi.waveform, c.decks[0].stem_wave.as_deref(), start_o, spb_o, vo.duration_secs);
    let cues = |info: &LoadedInfo| info.cues.iter().enumerate().filter_map(|(i, c)| c.as_ref().map(|c| (i, c.name.clone(), c.secs))).collect::<Vec<_>>();
    let (cues_o, cues_i) = (cues(oi), cues(ii));
    let (title_o, title_i) = (oi.tags.title.clone(), ii.tags.title.clone());
    let (key_o, key_i) = (oi.tags.key.map(|k| k.camelot()), ii.tags.key.map(|k| k.camelot()));
    let clash = c.keys_clash(1, 0);

    // ---- Every cue pair: the old track leaving from its cue, the new one coming in at its cue,
    // over the best length (the same score the search uses).
    let lim = Limits {
        earliest: 0,
        latest: outb.len(),
        lengths: (2..=32).map(|k| 4 * k).collect(),
        cue_bar: None,
        drop_cue_bar: None,
        in_cues: vec![],
        blend: true,
        cut: false,
        keys_clash: clash,
        vocals_out: mode == MixWhen::Now,
        stems: true,
        hurry: false,
    };
    let bar_of = |s: f64, b: &Bars| ((s - b.start) / b.bar_secs).round().max(0.0) as usize;
    let mut pairs = Vec::new();
    for (po, no, so) in &cues_o {
        for (pi, ni, si) in &cues_i {
            let (ob, ib) = (bar_of(*so, &outb), bar_of(*si, &inc));
            let mut best: Option<(f32, usize, Vec<String>)> = None;
            let mut by_len = Vec::new();
            for &bars in &lim.lengths {
                if ib + bars + 8 > inc.len() || ob + bars > outb.len() + 4 || ob + 2 > outb.len() {
                    continue;
                }
                let cand = Candidate { out_bar: ob, in_bar: ib, bars, technique: Technique::Blend, score: 0.0, notes: vec![] };
                let (s, notes) = tr::score(&outb, &inc, &cand, &lim);
                by_len.push(json!([bars, (s * 100.0).round() / 100.0]));
                if best.as_ref().is_none_or(|b| s > b.0) {
                    best = Some((s, bars, notes));
                }
            }
            pairs.push(json!({
                "out_pad": po + 1, "out_name": no, "out_secs": so, "out_bar": ob,
                "in_pad": pi + 1, "in_name": ni, "in_secs": si, "in_bar": ib,
                "best": best.as_ref().map(|b| json!({"score": b.0, "bars": b.1, "notes": b.2})),
                "by_length": by_len,
            }));
        }
    }

    // ---- dj's own choice (and its alternatives).
    let plans = c.search_plans(1, 0, mode, MixStyle::Auto);
    assert!(!plans.is_empty(), "no plan");
    let mut plan = plans[0].clone();
    // A chosen transition ("out pad,in pad,bars"), planned by dj's blend planner.
    let forced = std::env::var("MIX_FORCE").ok().map(|f| {
        let v: Vec<usize> = f.split(',').map(|x| x.trim().parse().unwrap()).collect();
        let (so, si) = (cues_o.iter().find(|c| c.0 + 1 == v[0]).unwrap().2, cues_i.iter().find(|c| c.0 + 1 == v[1]).unwrap().2);
        let (ob, ib, bars) = (bar_of(so, &outb), bar_of(si, &inc), v[2]);
        let w = |b: &Bars, from: usize| -> [Vec<f32>; 4] { std::array::from_fn(|p| (0..bars).map(|k| b.level(p, from + k)).collect()) };
        let blend = automix::plan_blend(&w(&inc, ib), &w(&outb, ob), bars, mode == MixWhen::Now, clash);
        let mut p = plan.clone();
        p.how = automix::MixHow::Stems;
        p.start_beat = ob as f64 * 4.0;
        p.length_beats = bars as f64 * 4.0;
        p.entry_secs = Some(inc.secs(ib as f64));
        p.lead_in = 0.0;
        p.filter = None;
        p.echo_at = None;
        p.reason = format!("chosen: out bar {}, in bar {}, {bars} bars", ob + 1, ib + 1);
        p.notes = blend.notes.clone();
        p.blend = Some(blend);
        p
    });
    if let Some(f) = forced.clone() {
        plan = f;
    }
    let report = json!({
        "a": {"path": a, "title": title_o, "bpm": vo.track_bpm, "key": key_o, "duration": vo.duration_secs, "analysis": bars_json(&outb, &cues_o)},
        "b": {"path": b, "title": title_i, "bpm": vi.track_bpm, "key": key_i, "duration": vi.duration_secs, "analysis": bars_json(&inc, &cues_i)},
        "keys_clash": clash,
        "mode": format!("{mode:?}"),
        "plans": plans.iter().map(plan_json).collect::<Vec<_>>(),
        "played": plan_json(&plan),
        "cue_pairs": pairs,
    });
    std::fs::write(out.join("analysis.json"), serde_json::to_string_pretty(&report).unwrap()).unwrap();
    eprintln!("plan: {:?} {} bars from beat {} ({})", plan.how, plan.length_beats / 4.0, plan.start_beat, plan.notes.join(" · "));

    // ---- Render: from 16 bars before the mix to 16 bars after it.
    let pre = 64.0;
    let from_beat = (plan.start_beat - plan.lead_in - pre).max(0.0);
    let from_secs = start_o + from_beat * spb_o;
    c.deck_cmd(0, DeckCmd::SeekFraction(from_secs / vo.duration_secs));
    for _ in 0..4 {
        pump(&mut e, &mut c, None);
    }
    let mix_end = plan.start_beat + plan.length_beats;
    c.start_mix(plan);
    let mut rec: Vec<[f32; 2]> = Vec::new();
    let mut timeline = Vec::new();
    let mut after_end: Option<f64> = None;
    loop {
        pump(&mut e, &mut c, Some(&mut rec));
        let t = rec.len() as f64 / SR;
        let ch = |k: usize| c.session.channels[k].params;
        let d = |k: usize| c.snap.decks[k];
        timeline.push(json!({
            "t": (t * 1000.0).round() / 1000.0,
            "beat_a": d(0).beat, "pos_a": d(0).position_secs, "pos_b": d(1).position_secs, "play_a": d(0).playing, "play_b": d(1).playing,
            "vol": [ch(0).volume, ch(1).volume], "eq_a": ch(0).eq, "eq_b": ch(1).eq, "cut_a": ch(0).cutoff,
            "stems_a": d(0).stem_gains, "stems_b": d(1).stem_gains, "mixing": c.automix.is_some(), "bpm": c.snap.clock.bpm,
        }));
        if c.automix.is_none() && after_end.is_none() {
            after_end = Some(t);
        }
        if after_end.is_some_and(|e| t > e + 16.0 * 4.0 * 60.0 / c.snap.clock.bpm.max(1.0)) || t > 900.0 {
            break;
        }
    }
    eprintln!("rendered {:.1} s; mix ended at {:?} s; mix beats to {}", rec.len() as f64 / SR, after_end, mix_end);
    std::fs::write(out.join("timeline.json"), serde_json::to_string(&json!({"from_secs_a": from_secs, "frames": timeline, "log": c.status.iter().map(|(_, m)| m.clone()).collect::<Vec<_>>()})).unwrap()).unwrap();
    write_wav(&out.join("mix.wav"), &rec);
}
