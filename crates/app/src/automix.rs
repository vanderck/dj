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
//! - **EQ**: the incoming volume opens quickly with its EQ closed; its highs come in, then its
//!   mids; at the midpoint the basses swap; then the outgoing track leaves on the EQ (mids, then
//!   highs) and its volume closes the last part.
//! - **Stems**: the incoming volume opens quickly on its melody (other) alone; then its drums come
//!   in; at the midpoint the basses swap (stems, so the kick stays whole); then the outgoing drums
//!   go, then its vocals and other. Volume only opens and closes the edges.
//!   With dj's own mixer the EQ smooths it: the new track's highs rise over the first half and the
//!   old track's highs roll off while its drums leave.
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
    /// The mix planned from both tracks' stems (`None` without stems: the fixed script).
    pub blend: Option<Blend>,
}

/// A mix in progress.
pub struct AutoMix {
    pub plan: MixPlan,
    released: bool,
    swapped: bool,
    /// Levels to restore afterwards (or on cancel).
    out_low: f32,
    in_low: f32,
    out_high: f32,
    out_mid: f32,
    in_high: f32,
    in_mid: f32,
    out_volume: f32,
    in_stems: [f32; 4],
    out_stems: [f32; 4],
    /// Now: the incoming vocals go out (and stay out after the mix).
    vocals_out: bool,
    /// The tracks on (incoming, outgoing) when the mix started.
    tracks: (u64, u64),
    /// External mixer: dj can't see or move the faders (no volume or EQ moves).
    external: bool,
    /// The outgoing deck's beat minus the master clock's when the mix started. The mix runs on
    /// the clock (+ this), which keeps counting while the outgoing deck loops.
    beat_offset: f64,
}

fn smooth(x: f64) -> f32 {
    let x = x.clamp(0.0, 1.0);
    (x * x * (3.0 - 2.0 * x)) as f32
}

/// Stem levels of a Stems blend at position `p` (0..1) of the mix, as fractions of the decks'
/// own levels: (incoming, outgoing). In: other, then drums, then (midpoint) bass. Out: bass at the
/// midpoint, then drums, then vocals and other.
fn stems_at(p: f64, vocals_out: bool, best: bool) -> ([f32; 4], [f32; 4]) {
    let mut i = [1.0, 0.0, 1.0, 1.0];
    let mut o = [1.0; 4];
    // The incoming melody (other) is there from the start; its drums come in next.
    i[DRUMS] = smooth((p - 0.15) / 0.2);
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
        // Waiting: paused (Mix loops it where it stands, at its cue), or looping at a cue.
        let waiting = !v.playing && !v.play_pending || (v.playing && v.loop_on);
        if !(v.loaded && v.has_grid && waiting) {
            return None;
        }
        if self.external_routing() {
            let playing = |o: usize| o != d && self.snap.decks[o].playing && self.snap.decks[o].has_grid && !self.snap.decks[o].loop_on;
            return match self.snap.clock.owner {
                ClockOwner::Deck(m) if playing(m as usize) => Some(m as usize),
                _ => (0..self.deck_count()).find(|&o| playing(o)),
            };
        }
        // A looping deck must be silent (else you're mixing it yourself); a paused one is made so.
        let p = &self.session.channels[d].params;
        if v.playing && p.volume > 0.02 && !p.mute {
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
        // A paused deck needs a moment to start in sync: one bar more.
        let lead = if self.snap.decks[incoming].playing { 0.5 } else { 4.5 };
        let next_bar = ((o.beat + lead) / 4.0).ceil() * 4.0;
        if how == MixHow::Drop {
            // On the next 8-bar phrase start when it's at most 4 bars away, else the next bar.
            let phrase = ((o.beat + lead) / 32.0).ceil() * 32.0;
            let (start_beat, reason) = if phrase - o.beat <= 16.0 { (phrase, "on the phrase") } else { (next_bar, "on the next bar") };
            return MixPlan { incoming, outgoing, mode: MixWhen::Now, how, start_beat, length_beats: DROP_BEATS, reason: reason.into(), why_how, blend: None };
        }
        let bar_s = 4.0 * 60.0 / o.bpm.max(1.0);
        let bars = (10.0 / bar_s).floor().clamp(2.0, 8.0);
        let vocals_out = self.snap.decks[incoming].stems_ready;
        let blend = self.analyse(incoming, outgoing, next_bar, bars as usize, vocals_out);
        MixPlan { incoming, outgoing, mode: MixWhen::Now, how, start_beat: next_bar, length_beats: bars * 4.0, reason: format!("{bars:.0} bars"), why_how, blend }
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
                if how == MixHow::Drop {
                    return MixPlan { incoming, outgoing, mode: MixWhen::Best, how, start_beat, length_beats: DROP_BEATS, reason, why_how, blend: None };
                }
                // How long: from the energy step and the new track's intro.
                let (bars, why_len) = self.best_length(incoming, outgoing, start_beat, end_beat);
                let reason = if why_len.is_empty() { format!("{bars} bars · {reason}") } else { format!("{bars} bars ({why_len}) · {reason}") };
                let blend = self.analyse(incoming, outgoing, start_beat, bars, false);
                MixPlan { incoming, outgoing, mode: MixWhen::Best, how, start_beat, length_beats: bars as f64 * 4.0, reason, why_how, blend }
            }
            // Not 16 bars left: mix now.
            None => self.plan_now(incoming, outgoing, style),
        }
    }

    /// Plan a blend of `bars` bars from `start` (outgoing beats) from both tracks' stems; `None`
    /// when either deck has none.
    pub fn analyse(&self, incoming: usize, outgoing: usize, start: f64, bars: usize, vocals_out: bool) -> Option<Blend> {
        let (wi, wo) = (self.decks[incoming].stem_wave.as_ref()?, self.decks[outgoing].stem_wave.as_ref()?);
        let (i, o) = (&self.snap.decks[incoming], &self.snap.decks[outgoing]);
        let (spb_i, spb_o) = (60.0 / i.track_bpm.max(1.0), 60.0 / o.track_bpm.max(1.0));
        // Where each track will be when the mix starts: the outgoing one plays on; the incoming
        // one starts from its loop (or, paused, from where it stands).
        let t_out = o.position_secs + (start - o.beat) * spb_o;
        let t_in = if i.loop_on { i.loop_start_secs } else { i.position_secs };
        let inc = bar_levels(wi, t_in, spb_i, bars);
        let out = bar_levels(wo, t_out, spb_o, bars);
        Some(plan_blend(&inc, &out, bars, vocals_out))
    }

    /// A deck's energy (tag, else dj's estimate).
    fn deck_energy(&self, d: usize) -> Option<u8> {
        let info = self.decks[d].info.as_ref()?;
        info.tags.energy.or_else(|| self.trackdb.tracks.get(&TrackDb::key(&info.path)).and_then(|o| o.energy))
    }

    /// Length of a Best blend: 16 bars; 8 when the energy steps up (2+ levels: get there
    /// quicker); 32 when it steps down, or when the new track's intro has no bass for 16 bars
    /// (a long, sparse intro to ride), if the outgoing track lasts that long.
    fn best_length(&self, incoming: usize, outgoing: usize, start: f64, end_beat: f64) -> (usize, String) {
        let fits32 = start + 128.0 <= end_beat;
        if let (Some(ei), Some(eo)) = (self.deck_energy(incoming), self.deck_energy(outgoing)) {
            let step = ei as i32 - eo as i32;
            if step >= 2 {
                return (8, format!("energy up {eo}→{ei}"));
            }
            if step <= -2 && fits32 {
                return (32, format!("energy down {eo}→{ei}"));
            }
        }
        if fits32 {
            if let Some(w) = self.decks[incoming].stem_wave.as_ref() {
                let i = &self.snap.decks[incoming];
                let t_in = if i.loop_on { i.loop_start_secs } else { i.position_secs };
                let bass = &bar_levels(w, t_in, 60.0 / i.track_bpm.max(1.0), 16)[BASS];
                if bass.iter().all(|x| *x <= PRESENT) {
                    return (32, "long intro".into());
                }
            }
        }
        (16, String::new())
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
        // A paused deck: silent (dj's mixer), a loop where it stands (its cue), and play in sync.
        if !self.snap.decks[i].playing {
            if !external {
                self.set_picked_up(&format!("Mixer/CH{}/Volume", i + 1), 0.0);
            }
            self.deck_cmd(i, DeckCmd::LoopBeats(self.loop_size[i]));
            self.deck_cmd(i, DeckCmd::Play);
        }
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
            out_high: params(o).eq[2],
            out_mid: params(o).eq[1],
            in_high: params(i).eq[2],
            in_mid: params(i).eq[1],
            out_volume: params(o).volume,
            in_stems: self.snap.decks[i].stem_gains,
            out_stems: self.snap.decks[o].stem_gains,
            vocals_out,
            tracks: (self.snap.decks[i].track_id, self.snap.decks[o].track_id),
            external,
            beat_offset: self.snap.decks[o].beat - self.snap.clock.beat,
            plan,
        };
        match mix.plan.how {
            // The incoming bass stays out until the swap.
            // The incoming track starts thinned out on the EQ: its parts come in one by one.
            MixHow::Eq if !external => {
                for band in ["Low", "Mid", "High"] {
                    self.mix_set(&format!("Mixer/CH{}/EQ/{band}", i + 1), 0.0);
                }
            }
            MixHow::Stems => {
                let si = match &mix.plan.blend {
                    Some(b) => b.levels(0.0, vocals_out).0,
                    None => stems_at(0.0, vocals_out, mix.plan.mode == MixWhen::Best).0,
                };
                self.set_stem_levels(i, si, mix.in_stems);
                // With dj's mixer the EQ shapes it too: the new track's highs come in gradually.
                if !external {
                    self.mix_set(&format!("Mixer/CH{}/EQ/High", i + 1), 0.0);
                }
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
                self.mix_set(&format!("Mixer/CH{}/EQ/Mid", i + 1), m.in_mid);
                self.mix_set(&format!("Mixer/CH{}/EQ/High", i + 1), m.in_high);
                self.mix_set(&format!("Mixer/CH{}/EQ/Low", o + 1), m.out_low);
                self.mix_set(&format!("Mixer/CH{}/EQ/Mid", o + 1), m.out_mid);
                self.mix_set(&format!("Mixer/CH{}/EQ/High", o + 1), m.out_high);
            }
            if !m.external && m.plan.how == MixHow::Stems {
                self.mix_set(&format!("Mixer/CH{}/EQ/High", i + 1), m.in_high);
                self.mix_set(&format!("Mixer/CH{}/EQ/High", o + 1), m.out_high);
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

    /// Where a running mix is, in the outgoing track's beats (counted on the clock).
    pub fn mix_beat(&self) -> f64 {
        self.automix.as_ref().map_or(0.0, |m| self.snap.clock.beat + m.beat_offset)
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
        if (vi.track_id, vo.track_id) != m.tracks || !vi.loaded || !vo.loaded || !vo.playing || !(vi.playing || vi.play_pending) {
            self.cancel_mix();
            return;
        }
        // The outgoing track's beats, counted on the clock: a loop on that deck doesn't stall the mix.
        let beat = self.snap.clock.beat + m.beat_offset;
        // Half a beat ahead: the incoming deck leaves its loop on the next beat, which is `start`.
        // If it hasn't started playing yet (a paused deck waits for the beat), a bar later.
        if !m.released && beat >= start - 0.5 && !vi.playing {
            self.automix.as_mut().unwrap().plan.start_beat += 4.0;
            return;
        }
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
        let (out_volume, in_low, out_low, in_high, out_high, out_mid, in_mid) =
            (m.out_volume, m.in_low, m.out_low, m.in_high, m.out_high, m.out_mid, m.in_mid);
        // Volume only opens the start (and closes the end): the EQ or stems do the blending.
        let open = smooth(p / 0.15);
        let blend = m.plan.blend.clone();
        let bp = p.min(1.0) * len / 4.0; // bar position in the mix
        let (in_stems, out_stems, vocals_out, best) = (m.in_stems, m.out_stems, m.vocals_out, m.plan.mode == MixWhen::Best);
        let vol = |c: usize| format!("Mixer/CH{}/Volume", c + 1);
        let low = |c: usize| format!("Mixer/CH{}/EQ/Low", c + 1);
        let high = |c: usize| format!("Mixer/CH{}/EQ/High", c + 1);
        let mid = |c: usize| format!("Mixer/CH{}/EQ/Mid", c + 1);
        match how {
            MixHow::Eq if external => {} // you fade on the mixer
            MixHow::Eq => {
                // In: volume opens, then highs, then mids; the bass swap (where the plan puts it:
                // where the new bass comes in or the old one stops, else halfway). Out: mids, then
                // highs; its volume only closes the last part.
                let ps = blend.as_ref().map_or(0.5, |b| b.swap_fraction()).clamp(0.2, 0.85);
                let after = 1.0 - ps;
                self.mix_set(&vol(i), open);
                if p < ps {
                    self.mix_set(&high(i), in_high * smooth((p - 0.2 * ps) / (0.5 * ps)));
                    self.mix_set(&mid(i), in_mid * smooth((p - 0.5 * ps) / (0.5 * ps)));
                }
                if p >= ps && !swapped {
                    self.automix.as_mut().unwrap().swapped = true;
                    self.mix_set(&low(i), in_low);
                    self.mix_set(&mid(i), in_mid);
                    self.mix_set(&high(i), in_high);
                    self.mix_set(&low(o), 0.0);
                }
                if p >= ps {
                    self.mix_set(&mid(o), out_mid * (1.0 - smooth((p - ps) / (0.6 * after))));
                    self.mix_set(&high(o), out_high * (1.0 - smooth((p - ps - 0.2 * after) / (0.6 * after))));
                    self.mix_set(&vol(o), out_volume * (1.0 - smooth((p - 0.85) / 0.15)));
                }
            }
            MixHow::Stems => {
                let (si, so) = match &blend {
                    Some(b) => b.levels(bp, vocals_out),
                    None => stems_at(p.min(1.0), vocals_out, best),
                };
                self.set_stem_levels(i, si, in_stems);
                self.set_stem_levels(o, so, out_stems);
                if !external {
                    // The outgoing track leaves on its stems; its volume stays up (it's at 0 at the end).
                    self.mix_set(&vol(i), open);
                    // The EQ smooths the stem swaps: the new highs rise over the first half; the
                    // old highs roll off while its drums leave.
                    self.mix_set(&high(i), in_high * smooth((p - 0.05) / 0.3));
                    self.mix_set(&high(o), out_high * (1.0 - smooth((p - 0.5) / 0.25)));
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
                // The old deck loops (silent with Stems or Drop) until you've faded it out: every
                // stem at 0, also those the plan left alone because they were silent in the mix.
                if how == MixHow::Stems && vo.stems_ready {
                    self.set_stem_levels(o, [0.0; 4], out_stems);
                }
                self.deck_cmd(o, DeckCmd::LoopBeats(self.loop_size[o]));
                self.log(format!("Deck {} is in, deck {} loops until you fade it out", i + 1, o + 1));
            } else {
                // Stop the old track: volume stays down; its bass and stems back for next time.
                self.deck_cmd(o, DeckCmd::Pause);
                self.mix_set(&vol(o), 0.0);
                if how == MixHow::Eq {
                    self.mix_set(&low(o), out_low);
                    self.mix_set(&mid(o), out_mid);
                    self.mix_set(&high(o), out_high);
                }
                if how == MixHow::Stems {
                    self.mix_set(&high(o), out_high);
                }
                if how != MixHow::Eq && vo.stems_ready {
                    self.set_stem_levels(o, [1.0; 4], out_stems);
                }
                self.log(format!("Mixed into deck {}", i + 1));
            }
            // The new track as it is from here: every stem in, except (Now) its vocals, which stay
            // out (bringing them back is yours).
            if how == MixHow::Stems {
                self.set_stem_levels(i, [1.0, 1.0, if vocals_out { 0.0 } else { 1.0 }, 1.0], in_stems);
            }
            self.automix = None;
        }
    }
}

/// A stem counts as playing in a bar when its level (relative to the track's loudest moments)
/// averages above this.
const PRESENT: f32 = 0.08;

/// A mix planned from the music: when each part of the two tracks comes in or leaves, from both
/// tracks' stem levels over the bars of the mix (see [`plan_blend`]).
#[derive(Clone, Debug, PartialEq)]
pub struct Blend {
    pub bars: usize,
    /// Mix bar where each incoming stem comes in (a one-bar ramp from there); `None` = left as the
    /// track plays (it's silent until it comes in on its own, or there from the start).
    pub in_at: [Option<f64>; 4],
    /// Mix bar by which each outgoing stem has left (a ramp over `out_ramp` bars before it);
    /// `None` = left alone (it stops on its own, or isn't there).
    pub out_at: [Option<f64>; 4],
    pub out_ramp: f64,
    /// Bar where the basses swap.
    pub swap: f64,
    /// What the plan follows, for the popup ("bass swaps at bar 9, where its bass comes in").
    pub notes: Vec<String>,
}

impl Blend {
    /// Stem levels (fractions of the decks' own) at bar position `bp` of the mix: (in, out).
    pub fn levels(&self, bp: f64, vocals_out: bool) -> ([f32; 4], [f32; 4]) {
        let mut i = [1.0f32; 4];
        let mut o = [1.0f32; 4];
        for s in 0..4 {
            if let Some(b) = self.in_at[s] {
                i[s] = if s == BASS { (bp >= b) as u8 as f32 } else { smooth(bp - b) };
            }
            if let Some(b) = self.out_at[s] {
                o[s] = if s == BASS { (bp < b) as u8 as f32 } else { 1.0 - smooth((bp - (b - self.out_ramp)) / self.out_ramp) };
            }
        }
        if vocals_out {
            i[VOCALS] = 0.0;
        }
        (i, o)
    }

    /// The swap as a fraction of the mix (for the EQ style).
    pub fn swap_fraction(&self) -> f64 {
        self.swap / self.bars as f64
    }
}

/// Per-bar stem levels: `bars` bars of `beat_secs` seconds × 4, from track time `t0`.
pub fn bar_levels(w: &crate::stems::StemWave, t0: f64, beat_secs: f64, bars: usize) -> [Vec<f32>; 4] {
    std::array::from_fn(|s| {
        let v = &w.levels[s];
        (0..bars)
            .map(|k| {
                let (a, b) = (t0 + k as f64 * 4.0 * beat_secs, t0 + (k + 1) as f64 * 4.0 * beat_secs);
                let (a, b) = (((a.max(0.0)) * w.bins_per_sec) as usize, ((b.max(0.0)) * w.bins_per_sec) as usize);
                let (a, b) = (a.min(v.len()), b.min(v.len()));
                if b > a { v[a..b].iter().sum::<f32>() / (b - a) as f32 } else { 0.0 }
            })
            .collect()
    })
}

/// Plan which part of each track moves when, from their per-bar stem levels over the mix
/// (`inc` and `out`, `bars` long; see [`bar_levels`]):
///
/// - The basses swap where the new bass comes in on its own, else where the old bass stops on
///   its own, else halfway.
/// - New drums that are there from the start come in halfway to the swap; drums that enter on
///   their own are left alone. Same for the melody (other), which is always let in.
/// - New vocals (Best) wait until the old vocals are gone.
/// - Old drums leave after the swap, halfway to the end; old vocals and other by the end; any
///   part that stops (or isn't there) on its own is left alone.
pub fn plan_blend(inc: &[Vec<f32>; 4], out: &[Vec<f32>; 4], bars: usize, vocals_out: bool) -> Blend {
    let bars = bars.max(2);
    let on = |v: &Vec<f32>, k: usize| v.get(k).is_some_and(|x| *x > PRESENT);
    // First bar it plays in, if it isn't there at the start.
    let enters = |v: &Vec<f32>| (!on(v, 0)).then(|| (1..bars).find(|&k| on(v, k))).flatten();
    // Bar from which it stays silent to the end, if it played before that.
    let stops = |v: &Vec<f32>| {
        let last = (0..bars).rev().find(|&k| on(v, k))?;
        (last + 1 < bars).then_some(last + 1)
    };
    let present = |v: &Vec<f32>| (0..bars).any(|k| on(v, k));
    let mut notes = Vec::new();

    let swap = match (enters(&inc[BASS]), stops(&out[BASS])) {
        (Some(e), _) if (1..bars).contains(&e) => {
            notes.push(format!("bass swaps at bar {}, where the new bass comes in", e + 1));
            e
        }
        (_, Some(x)) if (1..bars).contains(&x) => {
            notes.push(format!("bass swaps at bar {}, where the old bass stops", x + 1));
            x
        }
        _ => {
            let h = bars.div_ceil(2);
            notes.push(format!("bass swaps at bar {} (halfway)", h + 1));
            h
        }
    } as f64;

    let mut in_at = [None; 4];
    let mut out_at = [None; 4];
    in_at[BASS] = Some(swap);
    out_at[BASS] = Some(swap);
    // New drums: there from the start → bring them in halfway to the swap.
    if on(&inc[DRUMS], 0) {
        in_at[DRUMS] = Some((swap / 2.0).floor().max(1.0).min(swap));
    } else if enters(&inc[DRUMS]).is_some() {
        notes.push("new drums come in on their own".into());
    }
    // New vocals (Best): after the old ones.
    if !vocals_out && present(&inc[VOCALS]) && present(&out[VOCALS]) {
        let old_last = (0..bars).rev().find(|&k| on(&out[VOCALS], k)).unwrap_or(0);
        let wait = (old_last + 1).min(bars) as f64;
        in_at[VOCALS] = Some(wait);
        notes.push(format!("new vocals wait for bar {}", wait as usize + 1));
    }
    let out_ramp = if bars >= 8 { 2.0 } else { 1.0 };
    let rest = bars as f64 - swap;
    // Old drums: after the swap, unless they stop on their own.
    if present(&out[DRUMS]) && stops(&out[DRUMS]).is_none_or(|x| x as f64 > swap + rest / 2.0) {
        out_at[DRUMS] = Some((swap + (rest / 2.0).ceil()).min(bars as f64));
    } else if present(&out[DRUMS]) {
        notes.push("old drums stop on their own".into());
    }
    for s in [VOCALS, OTHER] {
        if !present(&out[s]) {
            if s == VOCALS {
                notes.push("no old vocals".into());
            }
            continue;
        }
        match stops(&out[s]) {
            Some(x) if x as f64 <= bars as f64 => {
                if s == VOCALS {
                    notes.push(format!("old vocals end at bar {}", x + 1));
                }
            }
            _ => out_at[s] = Some(bars as f64),
        }
    }
    Blend { bars, in_at, out_at, out_ramp, swap, notes }
}

#[cfg(test)]
mod tests {
    use super::{plan_blend, smooth, stems_at, BASS, DRUMS, OTHER, VOCALS};

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
        assert_eq!((i, o), ([0.0, 0.0, 0.0, 1.0], [1.0; 4]), "start: incoming melody (other) only");
        assert_eq!(stems_at(0.35, true, false).0[0], 1.0, "then its drums");
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

    /// Per-bar levels for 16 bars: (from, to) bar ranges where each stem plays.
    fn lv(ranges: [&[(usize, usize)]; 4]) -> [Vec<f32>; 4] {
        std::array::from_fn(|st| (0..16).map(|k| if ranges[st].iter().any(|&(a, b)| (a..b).contains(&k)) { 0.5 } else { 0.0 }).collect())
    }
    const ALL: &[(usize, usize)] = &[(0, 16)];
    const NONE: &[(usize, usize)] = &[];

    #[test]
    fn the_bass_swaps_where_the_music_says() {
        // The new bass comes in at bar 9 (index 8): swap there.
        let inc = lv([ALL, &[(8, 16)], NONE, ALL]);
        let out = lv([ALL, ALL, ALL, ALL]);
        let b = plan_blend(&inc, &out, 16, true);
        assert_eq!(b.swap, 8.0);
        assert!(b.notes[0].contains("new bass comes in"), "{:?}", b.notes);
        // The old bass stops at bar 7 (the new one is there from the start): swap there.
        let b = plan_blend(&lv([ALL, ALL, NONE, ALL]), &lv([ALL, &[(0, 6)], ALL, ALL]), 16, true);
        assert_eq!(b.swap, 6.0);
        assert!(b.notes[0].contains("old bass stops"));
        // Neither: halfway.
        assert_eq!(plan_blend(&lv([ALL, ALL, ALL, ALL]), &lv([ALL, ALL, ALL, ALL]), 16, true).swap, 8.0);
    }

    #[test]
    fn parts_that_move_on_their_own_are_left_alone() {
        // New drums only from bar 4, old drums stop at bar 5, no old vocals.
        let inc = lv([&[(3, 16)], ALL, NONE, ALL]);
        let out = lv([&[(0, 5)], ALL, NONE, ALL]);
        let b = plan_blend(&inc, &out, 16, true);
        assert_eq!(b.in_at[DRUMS], None, "new drums come in by themselves");
        assert_eq!(b.out_at[DRUMS], None, "old drums leave by themselves");
        assert_eq!(b.out_at[VOCALS], None);
        assert!(b.notes.iter().any(|n| n.contains("no old vocals")), "{:?}", b.notes);
        // Drums there from the start are brought in halfway to the swap; old drums leave after it.
        let b = plan_blend(&lv([ALL, ALL, NONE, ALL]), &lv([ALL, ALL, NONE, ALL]), 16, true);
        assert_eq!(b.in_at[DRUMS], Some(4.0));
        assert_eq!(b.out_at[DRUMS], Some(12.0));
        assert_eq!(b.out_at[OTHER], Some(16.0));
    }

    #[test]
    fn new_vocals_wait_for_the_old_ones() {
        let inc = lv([ALL, ALL, ALL, ALL]);
        let out = lv([ALL, ALL, &[(0, 11)], ALL]);
        let b = plan_blend(&inc, &out, 16, false);
        assert_eq!(b.in_at[VOCALS], Some(11.0));
        assert_eq!(b.out_at[VOCALS], None, "old vocals end on their own at bar 12");
        // Levels follow the plan: bass is a clean switch at the swap; others ramp.
        let (i, o) = b.levels(7.9, false);
        assert_eq!((i[BASS], o[BASS]), (0.0, 1.0));
        let (i, o) = b.levels(8.0, false);
        assert_eq!((i[BASS], o[BASS]), (1.0, 0.0));
        assert_eq!(b.levels(10.0, false).0[VOCALS], 0.0);
        assert_eq!(b.levels(12.0, false).0[VOCALS], 1.0);
        assert_eq!(b.levels(12.0, true).0[VOCALS], 0.0, "Now: vocals stay out");
    }
}
