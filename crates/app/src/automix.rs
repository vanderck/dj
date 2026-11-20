//! Auto-mix: bring a silent deck that loops at a cue into the playing track.
//!
//! The incoming deck loops at its cue with its volume down while another deck plays. Mix
//! (see `gui::deck`) switches its Sync on and either mixes **now** (on the next bar, within
//! about 10 s, the incoming track's vocals faded out until the outgoing one is gone) or at the
//! **best** moment (a 16-bar mix on a phrase start where the two tracks' vocals do not clash,
//! preferring the outgoing track's outro or break). With less than a minute of the outgoing
//! track left, it always mixes now.
//!
//! With an external mixer dj can't see or move the faders: Mix (Now or Best, as usual) syncs the
//! incoming deck and drops it out of its loop on the start beat, and at the end of the mix time
//! drops the outgoing deck into a loop, so both stay in time while you fade on the mixer. With Now
//! the incoming vocals stay out; bringing them back is up to you.
//!
//! The mix runs on the outgoing track's beats: the incoming track starts from its cue on the
//! first beat, its volume comes up over the first half, the basses swap at the midpoint, the
//! outgoing volume fades over the second half, then the outgoing deck stops. Every control dj
//! moves is picked up by MIDI ([`Core::set_picked_up`]).

use super::*;

/// Length of a "best" mix, in beats (16 bars).
pub const BEST_BEATS: f64 = 64.0;
/// Below this much outgoing time left, Mix always mixes now.
const LAST_MINUTE_SECS: f64 = 60.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MixWhen {
    Now,
    Best,
}

/// When and how a mix happens.
#[derive(Clone, Debug, PartialEq)]
pub struct MixPlan {
    pub incoming: usize,
    pub outgoing: usize,
    pub mode: MixWhen,
    /// Outgoing deck beat where the incoming track starts (a bar line).
    pub start_beat: f64,
    pub length_beats: f64,
    /// Why this moment, for the popup ("outro · no vocals in either").
    pub reason: String,
}

/// A mix in progress.
pub struct AutoMix {
    pub plan: MixPlan,
    released: bool,
    swapped: bool,
    /// The incoming deck's beat when the outgoing track stopped (its vocals then come back).
    finished: Option<f64>,
    /// Levels to restore afterwards: outgoing low EQ, incoming low EQ, incoming vocals.
    out_low: f32,
    in_low: f32,
    in_vocals: Option<f32>,
    out_volume: f32,
    /// The tracks on (incoming, outgoing) when the mix started.
    tracks: (u64, u64),
    /// External mixer: dj can't see or move the faders, so it only drops the decks in and out of
    /// loops on the beat and you fade on the mixer.
    external: bool,
}

fn smooth(x: f64) -> f32 {
    let x = x.clamp(0.0, 1.0);
    (x * x * (3.0 - 2.0 * x)) as f32
}

impl Core {
    /// The deck `d` could be mixed into (the playing deck it would replace), if `d` is ready:
    /// loaded, looping at a cue, silent (volume down) and synced or syncable, with another
    /// deck playing audibly. Not in external-mixer mode (the volumes are on the hardware).
    ///
    /// With an external mixer dj can't know the volumes: any deck looping at a cue while another
    /// plays is a candidate, and the outgoing deck is the master (or another playing deck).
    pub fn mix_target(&self, d: usize) -> Option<usize> {
        if self.automix.is_some() {
            return None;
        }
        let v = &self.snap.decks[d];
        if !(v.loaded && v.playing && v.loop_on && v.has_grid) {
            return None;
        }
        if self.external_routing() {
            let playing = |o: usize| o != d && self.snap.decks[o].playing && self.snap.decks[o].has_grid && !self.snap.decks[o].loop_on;
            return match self.snap.clock.owner {
                ClockOwner::Deck(m) if playing(m as usize) => Some(m as usize),
                _ => (0..self.deck_count()).find(|&o| playing(o)),
            };
        }
        let p = &self.session.channels[d].params;
        if p.volume > 0.02 && !p.mute {
            return None;
        }
        let audible = |o: usize| {
            let q = &self.session.channels[o].params;
            o != d && self.snap.decks[o].playing && self.snap.decks[o].has_grid && q.volume > 0.05 && !q.mute
        };
        match self.snap.clock.owner {
            ClockOwner::Deck(m) if audible(m as usize) => Some(m as usize),
            _ => (0..self.deck_count()).filter(|&o| audible(o)).max_by(|&a, &b| {
                self.session.channels[a].params.volume.total_cmp(&self.session.channels[b].params.volume)
            }),
        }
    }

    /// Whether Mix should skip the popup (the outgoing track ends within a minute).
    pub fn mix_is_urgent(&self, outgoing: usize) -> bool {
        let v = &self.snap.decks[outgoing];
        (v.duration_secs - v.position_secs) / v.speed.max(0.01) < LAST_MINUTE_SECS
    }

    /// Mix now: on the next bar line, over the whole bars that fit in about 10 s.
    pub fn plan_now(&self, incoming: usize, outgoing: usize) -> MixPlan {
        let o = &self.snap.decks[outgoing];
        let bar_s = 4.0 * 60.0 / o.bpm.max(1.0);
        let bars = (10.0 / bar_s).floor().clamp(2.0, 8.0);
        let start_beat = ((o.beat + 0.5) / 4.0).ceil() * 4.0;
        MixPlan { incoming, outgoing, mode: MixWhen::Now, start_beat, length_beats: bars * 4.0, reason: format!("{bars:.0} bars, vocals of the new track out") }
    }

    /// Mix at the best phrase start: 16 bars where the vocals clash least, preferring the
    /// outgoing track's outro or break, then sooner over later.
    pub fn plan_best(&self, incoming: usize, outgoing: usize) -> MixPlan {
        let o = &self.snap.decks[outgoing];
        let i = &self.snap.decks[incoming];
        let len = BEST_BEATS;
        // Beats left in the outgoing track (its own grid), and where its 8-bar phrases start.
        let spb_out = 60.0 / o.track_bpm.max(1.0);
        let end_beat = o.beat + (o.duration_secs - o.position_secs) / spb_out;
        let first = ((o.beat + 4.0) / 32.0).ceil() * 32.0;
        let out_secs = |beat: f64| o.position_secs + (beat - o.beat) * spb_out;
        let spb_in = 60.0 / i.track_bpm.max(1.0);
        let in_from = i.loop_start_secs;
        // Vocals while the two overlap (the whole mix), 0..1; unknown (no stems) = 0.5.
        let in_voc = self.vocal_level(incoming, in_from, in_from + len * spb_in).unwrap_or(0.5);
        let sections = self.decks[outgoing].info.as_ref().map(|inf| inf.cues.clone()).unwrap_or_default();
        let mut best: Option<(f32, f64, String)> = None;
        let mut b = first;
        while b + len <= end_beat {
            let (t0, t1) = (out_secs(b), out_secs(b + len));
            let out_voc = self.vocal_level(outgoing, t0, t1).unwrap_or(0.5);
            let clash = out_voc.min(in_voc);
            // A section cue (tags or detected) within a bar of here.
            let section = sections.iter().flatten().find(|c| (c.secs - t0).abs() < 4.0 * spb_out).map(|c| c.name.to_lowercase());
            let outro = section.as_deref().is_some_and(|n| n.contains("outro") || n.contains("break"));
            let wait = ((b - o.beat) / (end_beat - o.beat).max(1.0)) as f32;
            let phrase16 = (b / 64.0).fract().abs() < 1e-6;
            let score = 4.0 * clash + out_voc + 0.4 * wait - if outro { 0.6 } else { 0.0 } - if phrase16 { 0.1 } else { 0.0 };
            if best.as_ref().is_none_or(|x| score < x.0) {
                let mut why = Vec::new();
                if outro {
                    why.push(section.unwrap_or_default());
                }
                why.push(if clash < 0.15 { "no vocal clash".to_string() } else { "least vocal overlap".to_string() });
                best = Some((score, b, why.join(" · ")));
            }
            b += 32.0;
        }
        match best {
            Some((_, start_beat, reason)) => MixPlan { incoming, outgoing, mode: MixWhen::Best, start_beat, length_beats: len, reason },
            // Not 16 bars left: mix now.
            None => self.plan_now(incoming, outgoing),
        }
    }

    /// Mean vocal level (0..1, relative to the track's loudest moments) between two track
    /// times, from the deck's stems; `None` without stems.
    fn vocal_level(&self, d: usize, from: f64, to: f64) -> Option<f32> {
        let w = self.decks[d].stem_wave.as_ref()?;
        let v = &w.levels[2];
        let (a, b) = ((from.max(0.0) * w.bins_per_sec) as usize, ((to.max(0.0) * w.bins_per_sec) as usize).min(v.len()));
        (b > a).then(|| v[a..b].iter().sum::<f32>() / (b - a) as f32)
    }

    pub fn start_mix(&mut self, plan: MixPlan) {
        let (i, o) = (plan.incoming, plan.outgoing);
        // The playing track leads: if the silent deck is the tempo master, hand it over (at the
        // current tempo, nothing jumps); the incoming deck follows in sync.
        if self.snap.clock.owner == ClockOwner::Deck(i as u8) {
            self.deck_cmd(o, DeckCmd::ToggleMaster);
        }
        self.deck_cmd(i, DeckCmd::Sync(true));
        let external = self.external_routing();
        // Muted counts as silent: make it volume-down instead, so the fade-in is heard.
        if !external && self.session.channels[i].params.mute {
            self.set_channel(i, |p| {
                p.volume = 0.0;
                p.mute = false;
            });
        }
        let params = |c: usize| self.session.channels[c].params;
        let in_vocals = (plan.mode == MixWhen::Now && self.snap.decks[i].stems_ready).then(|| self.snap.decks[i].stem_gains[2]);
        let mix = AutoMix {
            released: false,
            swapped: false,
            finished: None,
            out_low: params(o).eq[0],
            in_low: params(i).eq[0],
            in_vocals,
            out_volume: params(o).volume,
            tracks: (self.snap.decks[i].track_id, self.snap.decks[o].track_id),
            external,
            plan,
        };
        // The incoming bass stays out until the swap; its vocals (Now) until the old track is gone.
        if !external {
            self.mix_set(&format!("Mixer/CH{}/EQ/Low", i + 1), 0.0);
        }
        if mix.in_vocals.is_some() {
            self.mix_set(&format!("Deck/{}/StemVocalsLevel", i + 1), 0.0);
        }
        self.log(format!(
            "Mixing deck {} into deck {}: {}",
            i + 1,
            o + 1,
            if mix.plan.mode == MixWhen::Now { "now".to_string() } else { format!("in {:.0} bars", ((mix.plan.start_beat - self.snap.decks[o].beat) / 4.0).max(0.0)) }
        ));
        self.automix = Some(mix);
    }

    pub fn cancel_mix(&mut self) {
        if let Some(m) = self.automix.take() {
            let (i, o) = (m.plan.incoming, m.plan.outgoing);
            if !m.external {
                if !m.swapped {
                    self.mix_set(&format!("Mixer/CH{}/EQ/Low", i + 1), m.in_low);
                }
                self.mix_set(&format!("Mixer/CH{}/EQ/Low", o + 1), m.out_low);
            }
            if let Some(v) = m.in_vocals {
                self.mix_set(&format!("Deck/{}/StemVocalsLevel", i + 1), v);
            }
            self.log("Mix cancelled".to_string());
        }
    }

    fn mix_set(&mut self, addr: &str, v: f32) {
        self.set_picked_up(addr, v);
    }

    /// Advance a running mix (called every tick).
    pub(super) fn automix_tick(&mut self) {
        let Some(m) = self.automix.as_ref() else { return };
        let (i, o, start, len) = (m.plan.incoming, m.plan.outgoing, m.plan.start_beat, m.plan.length_beats);
        let (vi, vo) = (self.snap.decks[i], self.snap.decks[o]);
        // Another track loaded (on the outgoing deck it only matters until that deck stopped).
        if vi.track_id != m.tracks.0 || (m.finished.is_none() && vo.track_id != m.tracks.1) {
            self.cancel_mix();
            return;
        }
        if let Some(since) = m.finished {
            // The old track is gone: bring the incoming track's vocals back over one bar.
            let x = (vi.beat - since) / 4.0;
            if !vi.playing || !vi.loaded {
                self.cancel_mix();
                return;
            }
            let target = m.in_vocals.unwrap_or(1.0);
            self.mix_set(&format!("Deck/{}/StemVocalsLevel", i + 1), target * smooth(x));
            if x >= 1.0 {
                self.automix = None;
            }
            return;
        }
        // A deck was stopped, ejected or replaced: give up (and put the EQ and vocals back).
        if !vi.loaded || !vo.loaded || !vo.playing || !vi.playing {
            self.cancel_mix();
            return;
        }
        let beat = vo.beat;
        let m = self.automix.as_mut().unwrap();
        // Half a beat ahead: the incoming deck leaves its loop on the next beat, which is `start`.
        if !m.released && beat >= start - 0.5 {
            m.released = true;
            self.deck_cmd(i, DeckCmd::ReleaseLoop);
            return;
        }
        if beat < start {
            return;
        }
        let p = (beat - start) / len;
        if m.external {
            // You fade on the mixer. When the mix time is up, the old deck drops into a loop (the
            // deck's loop size) so it stays in time until you've faded it out.
            if p >= 1.0 {
                self.deck_cmd(o, DeckCmd::LoopBeats(self.loop_size[o]));
                self.log(format!("Deck {} is in, deck {} loops until you fade it out", i + 1, o + 1));
                // The vocals stay out: dj can't tell when you've faded the old track out, so
                // bringing them back is yours (the deck's stems).
                self.automix = None;
            }
            return;
        }
        let (out_volume, in_low, out_low, swapped) = (m.out_volume, m.in_low, m.out_low, m.swapped);
        // First half: incoming up. Midpoint: swap the basses. Second half: outgoing down.
        self.mix_set(&format!("Mixer/CH{}/Volume", i + 1), smooth(p / 0.5));
        if p >= 0.5 && !swapped {
            self.automix.as_mut().unwrap().swapped = true;
            self.mix_set(&format!("Mixer/CH{}/EQ/Low", i + 1), in_low);
            self.mix_set(&format!("Mixer/CH{}/EQ/Low", o + 1), 0.0);
        }
        if p >= 0.5 {
            self.mix_set(&format!("Mixer/CH{}/Volume", o + 1), out_volume * (1.0 - smooth((p - 0.5) / 0.5)));
        }
        if p >= 1.0 {
            // Done: stop the old track (volume stays down, its bass back for next time).
            self.deck_cmd(o, DeckCmd::Pause);
            self.mix_set(&format!("Mixer/CH{}/Volume", o + 1), 0.0);
            self.mix_set(&format!("Mixer/CH{}/EQ/Low", o + 1), out_low);
            self.log(format!("Mixed into deck {}", i + 1));
            let m = self.automix.as_mut().unwrap();
            if m.in_vocals.is_some() {
                m.finished = Some(vi.beat);
            } else {
                self.automix = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::smooth;

    #[test]
    fn fades_are_smooth_and_bounded() {
        assert_eq!(smooth(-1.0), 0.0);
        assert_eq!(smooth(0.0), 0.0);
        assert!((smooth(0.5) - 0.5).abs() < 1e-6);
        assert_eq!(smooth(1.0), 1.0);
        assert_eq!(smooth(2.0), 1.0);
    }
}
