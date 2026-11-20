//! Auto-mix: bring a deck that loops at a cue into the playing track.
//!
//! The incoming deck loops at its cue (volume down, with dj's own mixer) while another deck
//! plays. Mix (see `gui::deck`) switches its Sync on and picks **when**: now (the next bar,
//! within about 10 s, the incoming track's vocals taken out for good) or at the best moment (a
//! phrase start where the two tracks' vocals clash least, preferring the outgoing track's outro or
//! break). With less than a minute of the outgoing track left it always mixes now.
//!
//! And **how** ([`MixStyle`], remembered in the settings):
//!
//! - **EQ**: the incoming track comes up over the first half with its bass cut, the basses swap
//!   on the EQ at the midpoint, the outgoing track fades over the second half and stops.
//! - **Stems**: the incoming track comes in with drums and other only; at the midpoint the basses
//!   swap (stems, so the kick stays whole), then the outgoing drums go, then its vocals and other.
//! - **Drop**: no blend. The incoming track waits at its drop cue (if it has one) and cuts in on
//!   the start beat while the outgoing one is cut within a beat.
//! - **Auto**: dj assesses the two tracks ([`Core::assess`]): Drop when their keys clash or their
//!   tempos are far apart (no long overlap), or for a Best mix when the incoming track has a drop
//!   cue; else Stems when both decks have stems; else EQ.
//!
//! With an external mixer dj can't see or move the faders: no volume or EQ moves. Stems and Drop
//! still blend on the decks; EQ means you fade on the mixer. At the end the outgoing deck drops
//! into a loop, so it stays in time until you've faded it out.
//!
//! Everything runs on the outgoing track's beats; every control dj moves is picked up by MIDI
//! ([`Core::set_picked_up`]).

use super::*;
pub use crate::settings::MixStyle;

/// Length of a "best" blend, in beats (16 bars).
pub const BEST_BEATS: f64 = 64.0;
/// A drop: one bar.
const DROP_BEATS: f64 = 4.0;
/// Below this much outgoing time left, Mix always mixes now.
const LAST_MINUTE_SECS: f64 = 60.0;
/// Stem order: drums, bass, vocals, other.
const DRUMS: usize = 0;
const BASS: usize = 1;
const VOCALS: usize = 2;
const OTHER: usize = 3;
const STEM_ADDR: [&str; 4] = ["StemDrumsLevel", "StemBassLevel", "StemVocalsLevel", "StemOtherLevel"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MixWhen {
    Now,
    Best,
}

/// How a mix blends, once [`MixStyle::Auto`] is decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MixHow {
    Eq,
    Stems,
    Drop,
}

impl MixHow {
    pub fn name(self, external: bool) -> &'static str {
        match self {
            MixHow::Eq if external => "You fade",
            MixHow::Eq => "EQ",
            MixHow::Stems => "Stems",
            MixHow::Drop => "Drop",
        }
    }
}

/// When and how a mix happens.
#[derive(Clone, Debug, PartialEq)]
pub struct MixPlan {
    pub incoming: usize,
    pub outgoing: usize,
    pub mode: MixWhen,
    pub how: MixHow,
    /// Outgoing deck beat where the incoming track starts (a bar line).
    pub start_beat: f64,
    pub length_beats: f64,
    /// Why this moment, for the popup ("outro · no vocal clash").
    pub reason: String,
    /// Why this style, when Auto chose it ("keys clash", "vocals overlap").
    pub why_how: String,
}

/// A mix in progress.
pub struct AutoMix {
    pub plan: MixPlan,
    released: bool,
    swapped: bool,
    /// Levels to restore afterwards (or on cancel).
    out_low: f32,
    in_low: f32,
    out_volume: f32,
    in_stems: [f32; 4],
    out_stems: [f32; 4],
    /// Now: the incoming vocals go out (and stay out after the mix).
    vocals_out: bool,
    /// The tracks on (incoming, outgoing) when the mix started.
    tracks: (u64, u64),
    /// External mixer: dj can't see or move the faders (no volume or EQ moves).
    external: bool,
}

fn smooth(x: f64) -> f32 {
    let x = x.clamp(0.0, 1.0);
    (x * x * (3.0 - 2.0 * x)) as f32
}

/// Stem levels of a Stems blend at position `p` (0..1) of the mix, as fractions of the decks'
/// own levels: (incoming, outgoing).
fn stems_at(p: f64, vocals_out: bool, best: bool) -> ([f32; 4], [f32; 4]) {
    let mut i = [1.0, 0.0, 1.0, 1.0];
    let mut o = [1.0; 4];
    if p >= 0.5 {
        // Midpoint: the basses swap.
        i[BASS] = 1.0;
        o[BASS] = 0.0;
    }
    // Then the outgoing drums go, then its vocals and other.
    o[DRUMS] = 1.0 - smooth((p - 0.5) / 0.25);
    let tail = 1.0 - smooth((p - 0.75) / 0.25);
    o[VOCALS] = tail;
    o[OTHER] = tail;
    // Incoming vocals: out for Now; for Best they come in as the outgoing ones leave.
    i[VOCALS] = if vocals_out {
        0.0
    } else if best {
        smooth((p - 0.75) / 0.25)
    } else {
        1.0
    };
    (i, o)
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

    /// The incoming deck's drop cue (a hot cue named "Drop…"), if any.
    pub fn drop_cue(&self, d: usize) -> Option<u8> {
        let cues = &self.decks[d].info.as_ref()?.cues;
        cues.iter().position(|c| c.as_ref().is_some_and(|c| c.name.to_lowercase().contains("drop"))).map(|i| i as u8)
    }

    /// What a style means for these two decks (Stems needs stems on both; Auto assesses the
    /// tracks), and why.
    pub fn resolve_style(&self, style: MixStyle, incoming: usize, outgoing: usize, when: MixWhen) -> (MixHow, String) {
        let stems = self.snap.decks[incoming].stems_ready && self.snap.decks[outgoing].stems_ready;
        match style {
            MixStyle::Eq => (MixHow::Eq, String::new()),
            MixStyle::Drop => (MixHow::Drop, String::new()),
            MixStyle::Stems if stems => (MixHow::Stems, String::new()),
            MixStyle::Stems => (MixHow::Eq, "no stems yet".into()),
            MixStyle::Auto => self.assess(incoming, outgoing, when),
        }
    }

    /// Auto: the best way to mix these two tracks.
    ///
    /// - Keys that clash, or tempos far apart (a long overlap would sound off, or stretch one
    ///   track a lot): Drop, so they barely overlap.
    /// - The new track has a drop cue and the mix waits for the best moment: Drop, on the phrase.
    /// - Stems on both decks: Stems (the parts swap one by one, vocals never on top of vocals).
    /// - Otherwise: EQ.
    pub fn assess(&self, incoming: usize, outgoing: usize, when: MixWhen) -> (MixHow, String) {
        use dj_library::key::Compat;
        let (vi, vo) = (&self.snap.decks[incoming], &self.snap.decks[outgoing]);
        let clash = match (self.deck_key(incoming), self.deck_key(outgoing)) {
            (Some(a), Some(b)) => matches!(a.compat(b), Compat::Clash | Compat::Diagonal),
            _ => false,
        };
        if clash {
            return (MixHow::Drop, "keys clash".into());
        }
        let stretch = (vi.track_bpm / vo.bpm.max(1.0) - 1.0).abs();
        if vi.has_grid && stretch > 0.08 {
            return (MixHow::Drop, format!("tempos {:.0} % apart", stretch * 100.0));
        }
        if when == MixWhen::Best && self.drop_cue(incoming).is_some() {
            return (MixHow::Drop, "the new track has a drop".into());
        }
        if vi.stems_ready && vo.stems_ready {
            // Vocals in both around the mix: stems keep them apart.
            let spb_in = 60.0 / vi.track_bpm.max(1.0);
            let in_voc = self.vocal_level(incoming, vi.loop_start_secs, vi.loop_start_secs + 32.0 * spb_in).unwrap_or(0.0);
            let out_voc = self.vocal_level(outgoing, vo.position_secs, vo.position_secs + 30.0).unwrap_or(0.0);
            let why = if in_voc.min(out_voc) > 0.15 { "vocals overlap" } else { "stems on both decks" };
            return (MixHow::Stems, why.into());
        }
        (MixHow::Eq, "no stems yet".into())
    }

    /// Mix now: a blend from the next bar line over the whole bars that fit in about 10 s; a drop
    /// on the next 8-bar phrase start (or the next bar when the track ends sooner).
    pub fn plan_now(&self, incoming: usize, outgoing: usize, style: MixStyle) -> MixPlan {
        let o = &self.snap.decks[outgoing];
        let (how, why_how) = self.resolve_style(style, incoming, outgoing, MixWhen::Now);
        let next_bar = ((o.beat + 0.5) / 4.0).ceil() * 4.0;
        if how == MixHow::Drop {
            // On the next 8-bar phrase start when it's at most 4 bars away, else the next bar.
            let phrase = ((o.beat + 0.5) / 32.0).ceil() * 32.0;
            let (start_beat, reason) = if phrase - o.beat <= 16.0 { (phrase, "on the phrase") } else { (next_bar, "on the next bar") };
            return MixPlan { incoming, outgoing, mode: MixWhen::Now, how, start_beat, length_beats: DROP_BEATS, reason: reason.into(), why_how };
        }
        let bar_s = 4.0 * 60.0 / o.bpm.max(1.0);
        let bars = (10.0 / bar_s).floor().clamp(2.0, 8.0);
        MixPlan { incoming, outgoing, mode: MixWhen::Now, how, start_beat: next_bar, length_beats: bars * 4.0, reason: format!("{bars:.0} bars"), why_how }
    }

    /// Mix at the best phrase start: where the vocals clash least over 16 bars, preferring the
    /// outgoing track's outro or break, then sooner over later.
    pub fn plan_best(&self, incoming: usize, outgoing: usize, style: MixStyle) -> MixPlan {
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
            Some((_, start_beat, reason)) => {
                let (how, why_how) = self.resolve_style(style, incoming, outgoing, MixWhen::Best);
                let length_beats = if how == MixHow::Drop { DROP_BEATS } else { len };
                MixPlan { incoming, outgoing, mode: MixWhen::Best, how, start_beat, length_beats, reason, why_how }
            }
            // Not 16 bars left: mix now.
            None => self.plan_now(incoming, outgoing, style),
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
        // The playing track leads: if the waiting deck is the tempo master, hand it over (at the
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
        let vocals_out = plan.mode == MixWhen::Now && self.snap.decks[i].stems_ready;
        let mix = AutoMix {
            released: false,
            swapped: false,
            out_low: params(o).eq[0],
            in_low: params(i).eq[0],
            out_volume: params(o).volume,
            in_stems: self.snap.decks[i].stem_gains,
            out_stems: self.snap.decks[o].stem_gains,
            vocals_out,
            tracks: (self.snap.decks[i].track_id, self.snap.decks[o].track_id),
            external,
            plan,
        };
        match mix.plan.how {
            // The incoming bass stays out until the swap.
            MixHow::Eq if !external => self.mix_set(&format!("Mixer/CH{}/EQ/Low", i + 1), 0.0),
            MixHow::Stems => {
                let (si, _) = stems_at(0.0, vocals_out, mix.plan.mode == MixWhen::Best);
                self.set_stem_levels(i, si, mix.in_stems);
            }
            _ => {}
        }
        if vocals_out {
            self.mix_set(&format!("Deck/{}/StemVocalsLevel", i + 1), 0.0);
        }
        let when = if mix.plan.mode == MixWhen::Now && mix.plan.start_beat - self.snap.decks[o].beat <= 4.0 {
            "now".to_string()
        } else {
            format!("in {:.0} bars", ((mix.plan.start_beat - self.snap.decks[o].beat) / 4.0).max(0.0))
        };
        self.log(format!("Mixing deck {} into deck {} ({}): {when}", i + 1, o + 1, mix.plan.how.name(external)));
        self.automix = Some(mix);
    }

    /// Set a deck's four stem levels to `frac` of `base`.
    fn set_stem_levels(&mut self, d: usize, frac: [f32; 4], base: [f32; 4]) {
        for s in 0..4 {
            self.mix_set(&format!("Deck/{}/{}", d + 1, STEM_ADDR[s]), frac[s] * base[s]);
        }
    }

    pub fn cancel_mix(&mut self) {
        if let Some(m) = self.automix.take() {
            let (i, o) = (m.plan.incoming, m.plan.outgoing);
            if !m.external && m.plan.how == MixHow::Eq {
                if !m.swapped {
                    self.mix_set(&format!("Mixer/CH{}/EQ/Low", i + 1), m.in_low);
                }
                self.mix_set(&format!("Mixer/CH{}/EQ/Low", o + 1), m.out_low);
            }
            if self.snap.decks[i].stems_ready {
                self.set_stem_levels(i, [1.0; 4], m.in_stems);
            }
            if self.snap.decks[o].stems_ready && m.plan.how != MixHow::Eq {
                self.set_stem_levels(o, [1.0; 4], m.out_stems);
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
        // Another track loaded on either deck, or a deck stopped or ejected: give up (and put the
        // EQ and stems back).
        if (vi.track_id, vo.track_id) != m.tracks || !vi.loaded || !vo.loaded || !vo.playing || !vi.playing {
            self.cancel_mix();
            return;
        }
        let beat = vo.beat;
        // Half a beat ahead: the incoming deck leaves its loop on the next beat, which is `start`.
        if !m.released && beat >= start - 0.5 {
            // A drop starts from the incoming track's drop cue.
            let to_cue = if m.plan.how == MixHow::Drop { self.drop_cue(i) } else { None };
            self.automix.as_mut().unwrap().released = true;
            self.deck_cmd(i, DeckCmd::ReleaseLoop { to_cue });
            return;
        }
        if beat < start {
            return;
        }
        let p = (beat - start) / len;
        let (how, external, swapped) = (m.plan.how, m.external, m.swapped);
        let (out_volume, in_low, out_low) = (m.out_volume, m.in_low, m.out_low);
        let (in_stems, out_stems, vocals_out, best) = (m.in_stems, m.out_stems, m.vocals_out, m.plan.mode == MixWhen::Best);
        let vol = |c: usize| format!("Mixer/CH{}/Volume", c + 1);
        let low = |c: usize| format!("Mixer/CH{}/EQ/Low", c + 1);
        match how {
            MixHow::Eq if external => {} // you fade on the mixer
            MixHow::Eq => {
                // First half: incoming up. Midpoint: swap the basses. Second half: outgoing down.
                self.mix_set(&vol(i), smooth(p / 0.5));
                if p >= 0.5 && !swapped {
                    self.automix.as_mut().unwrap().swapped = true;
                    self.mix_set(&low(i), in_low);
                    self.mix_set(&low(o), 0.0);
                }
                if p >= 0.5 {
                    self.mix_set(&vol(o), out_volume * (1.0 - smooth((p - 0.5) / 0.5)));
                }
            }
            MixHow::Stems => {
                let (si, so) = stems_at(p.min(1.0), vocals_out, best);
                self.set_stem_levels(i, si, in_stems);
                self.set_stem_levels(o, so, out_stems);
                if !external {
                    self.mix_set(&vol(i), smooth(p / 0.5));
                    self.mix_set(&vol(o), out_volume * (1.0 - smooth((p - 0.75) / 0.25)));
                }
            }
            MixHow::Drop => {
                // The new track at full straight away; the old one gone within a beat.
                let cut = 1.0 - smooth(p * len);
                if external {
                    if vo.stems_ready {
                        self.set_stem_levels(o, [cut; 4], out_stems);
                    }
                } else {
                    self.mix_set(&vol(i), 1.0);
                    self.mix_set(&vol(o), out_volume * cut);
                }
            }
        }
        if p >= 1.0 {
            if external {
                // The old deck loops (silent with Stems or Drop) until you've faded it out.
                self.deck_cmd(o, DeckCmd::LoopBeats(self.loop_size[o]));
                self.log(format!("Deck {} is in, deck {} loops until you fade it out", i + 1, o + 1));
            } else {
                // Stop the old track: volume stays down; its bass and stems back for next time.
                self.deck_cmd(o, DeckCmd::Pause);
                self.mix_set(&vol(o), 0.0);
                if how == MixHow::Eq {
                    self.mix_set(&low(o), out_low);
                }
                if how != MixHow::Eq && vo.stems_ready {
                    self.set_stem_levels(o, [1.0; 4], out_stems);
                }
                self.log(format!("Mixed into deck {}", i + 1));
            }
            // With Now, the new track's vocals stay out: bringing them back is yours.
            self.automix = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{smooth, stems_at};

    #[test]
    fn fades_are_smooth_and_bounded() {
        assert_eq!(smooth(-1.0), 0.0);
        assert_eq!(smooth(0.0), 0.0);
        assert!((smooth(0.5) - 0.5).abs() < 1e-6);
        assert_eq!(smooth(1.0), 1.0);
        assert_eq!(smooth(2.0), 1.0);
    }

    #[test]
    fn stems_blend_swaps_bass_then_drums_then_the_rest() {
        let (i, o) = stems_at(0.0, true, false);
        assert_eq!((i, o), ([1.0, 0.0, 0.0, 1.0], [1.0; 4]), "start: incoming drums and other only");
        let (i, o) = stems_at(0.5, true, false);
        assert_eq!((i[1], o[1]), (1.0, 0.0), "midpoint: the basses swap");
        assert_eq!(o[0], 1.0);
        let (_, o) = stems_at(0.75, true, false);
        assert_eq!((o[0], o[2], o[3]), (0.0, 1.0, 1.0), "outgoing drums gone, then vocals and other");
        let (i, o) = stems_at(1.0, true, false);
        assert_eq!(o, [0.0; 4]);
        assert_eq!(i, [1.0, 1.0, 0.0, 1.0], "Now: vocals stay out");
        assert_eq!(stems_at(1.0, false, true).0[2], 1.0, "Best: incoming vocals come in at the end");
        assert_eq!(stems_at(0.5, false, true).0[2], 0.0);
    }
}
