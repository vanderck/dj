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
/// Without analysis (no beat grid or waveform), a Now mix lasts about this long (8–16 bars).
const NOW_SECS: f64 = 32.0;
/// The shortest and longest blend the search tries, in bars.
const MIN_MIX_BARS: usize = 8;
const MAX_MIX_BARS: usize = 128;
/// In the playing track's last minute, Mix doesn't ask: it mixes at the best moment left.
const LAST_MINUTE_SECS: f64 = 60.0;
/// Stem order: drums, bass, vocals, other.
use crate::transition::{BASS, DRUMS, OTHER, PRESENT, VOCALS};
/// The echo an echo-out inserts.
const ECHO_ID: &str = "native:echo";
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
    /// Why this moment ("outro · no vocal clash"), printed when the mix starts.
    pub reason: String,
    /// Why this style, when Auto chose it ("keys clash", "vocals overlap").
    pub why_how: String,
    /// The mix planned from both tracks' stems (`None` without stems: the fixed script).
    pub blend: Option<Blend>,
    /// Beats before `start_beat` that the incoming track starts, so its own bar lines fall on
    /// the outgoing track's (its cue isn't always on a downbeat).
    pub lead_in: f64,
    /// Where the incoming track starts (track seconds); `None` = its loop start (or drop cue).
    pub entry_secs: Option<f64>,
    /// A high-pass sweep on the old track over these bars of the mix (it's still full there).
    pub filter: Option<(f64, f64)>,
    /// An echo-out on the old track from this bar (it has to leave mid-phrase).
    pub echo_at: Option<f64>,
    /// What the plan follows in the two tracks, printed when the mix starts.
    pub notes: Vec<String>,
    /// Semitones the new track is shifted so its key fits (0: it already does).
    pub transpose: i32,
}

impl MixPlan {
    /// The extra fields of a fixed-script plan (no search).
    fn plain() -> Self {
        MixPlan {
            incoming: 0,
            outgoing: 0,
            mode: MixWhen::Now,
            how: MixHow::Eq,
            start_beat: 0.0,
            length_beats: 0.0,
            reason: String::new(),
            why_how: String::new(),
            blend: None,
            lead_in: 0.0,
            entry_secs: None,
            filter: None,
            echo_at: None,
            notes: Vec::new(),
            transpose: 0,
        }
    }
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
    /// The old channel's filter before the mix (a sweep puts it back) and the echo inserted on it.
    out_cutoff: f32,
    echo_slot: Option<usize>,
    /// The new deck's transposition before the mix (put back on cancel).
    in_semitones: f64,
}

/// After a Now mix: bring the new track's vocals back where its next vocal phrase starts.
#[derive(Clone, Copy, Debug)]
pub struct VocalReturn {
    deck: usize,
    track_id: u64,
    /// Clock beat where they come back (over one bar).
    at_clock: f64,
    level: f32,
}

/// After a mix the new track plays at the old one's tempo; it eases back to its own.
#[derive(Clone, Copy, Debug)]
pub struct TempoHome {
    deck: usize,
    track_id: u64,
    /// The fader dj set last, the clock beat then, and the fader before it (the deck may not show
    /// the newest yet); any other: you moved it.
    last: Option<(f64, f64, f64)>,
}

impl TempoHome {
    pub fn new(deck: usize, track_id: u64) -> Self {
        Self { deck, track_id, last: None }
    }
}

/// How fast the tempo eases home: 0.25 % a bar, too slow to hear (8 % takes 32 bars).
const TEMPO_HOME_PER_BAR: f64 = 0.0025;

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
    // The incoming melody (other) rises from the start; its drums come in next.
    i[OTHER] = smooth(p / 0.4);
    i[DRUMS] = smooth((p - 0.15) / 0.3);
    if p >= 0.5 {
        // Midpoint: the basses swap.
        i[BASS] = 1.0;
        o[BASS] = 0.0;
    }
    // Then the outgoing drums go, then its vocals and other: out by three quarters (its volume
    // closes after that).
    o[DRUMS] = 1.0 - smooth((p - 0.5) / 0.15);
    let tail = 1.0 - smooth((p - 0.5) / 0.25);
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
        let external = self.external_routing();
        // Waiting: paused (Mix loops it where it stands), looping at a cue, or (dj's mixer) just
        // turned down: volume down is enough, it may be playing in your headphones.
        let p = &self.session.channels[d].params;
        let silent = p.volume <= 0.02 || p.mute;
        let waiting = (!v.playing && !v.play_pending) || (v.playing && v.loop_on) || (!external && v.playing && silent);
        if !(v.loaded && v.has_grid && waiting) {
            return None;
        }
        if external {
            let playing = |o: usize| o != d && self.snap.decks[o].playing && self.snap.decks[o].has_grid && !self.snap.decks[o].loop_on;
            return match self.snap.clock.owner {
                ClockOwner::Deck(m) if playing(m as usize) => Some(m as usize),
                _ => (0..self.deck_count()).find(|&o| playing(o)),
            };
        }
        // A playing deck must be silent (else you're mixing it yourself); a paused one is made so.
        if v.playing && !silent {
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

    /// Whether Mix should skip the popup (the playing track ends within a minute).
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
    /// - Keys that clash: with stems, Stems with only drums and bass overlapping (the melodies
    ///   switch at the swap); without, Drop. Tempos far apart (a long overlap would stretch one
    ///   track a lot): Drop, so they barely overlap.
    /// - The new track has a drop cue and the mix waits for the best moment: Drop, on the phrase.
    /// - Stems on both decks: Stems (the parts swap one by one, vocals never on top of vocals).
    /// - Otherwise: EQ.
    pub fn assess(&self, incoming: usize, outgoing: usize, when: MixWhen) -> (MixHow, String) {
        let (vi, vo) = (&self.snap.decks[incoming], &self.snap.decks[outgoing]);
        // (A small transposition fixes a near miss: then they don't clash.)
        let clash = self.keys_clash(incoming, outgoing) && self.key_shift(incoming, outgoing).is_none();
        if clash && vi.stems_ready && vo.stems_ready {
            // Only drums and bass overlap; the melodies and vocals switch at the swap.
            return (MixHow::Stems, "keys clash: melodies switch at the swap".into());
        }
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

    /// When the keys clash: a shift of the new track by one semitone that puts it in a key that
    /// mixes with the playing one (same, adjacent or relative), if any. (One semitone keeps a
    /// track sounding like itself, and fixes 8 of 12 keys; with two, every clash would be
    /// "fixed" by changing the track's character.)
    pub fn key_shift(&self, incoming: usize, outgoing: usize) -> Option<i32> {
        use dj_library::key::Compat;
        let (a, b) = (self.deck_key(incoming)?, self.deck_key(outgoing)?);
        if !self.keys_clash(incoming, outgoing) {
            return None;
        }
        [1, -1].into_iter().find(|s| matches!(a.transpose(*s).compat(b), Compat::Same | Compat::Adjacent | Compat::Relative))
    }

    /// The two decks' keys don't mix (clash or diagonal on the Camelot wheel).
    pub fn keys_clash(&self, a: usize, b: usize) -> bool {
        use dj_library::key::Compat;
        match (self.deck_key(a), self.deck_key(b)) {
            (Some(x), Some(y)) => matches!(x.compat(y), Compat::Clash | Compat::Diagonal),
            _ => false,
        }
    }

    /// Beats the incoming track must start before a bar line of the mix so its own bar lines
    /// line up: its start point (loop start, or the drop cue) is `k` beats into a bar of its grid,
    /// so it starts `4 - k` beats early (0 when it's on a downbeat).
    fn lead_in(&self, incoming: usize, from_secs: f64) -> f64 {
        let i = &self.snap.decks[incoming];
        if !i.has_grid || i.track_bpm <= 0.0 {
            return 0.0;
        }
        let beat = i.beat + (from_secs - i.position_secs) * i.track_bpm / 60.0;
        let k = beat.rem_euclid(4.0).round() % 4.0;
        (4.0 - k) % 4.0
    }

    /// Where the incoming track will start playing (its loop, or where it stands; a drop from its
    /// drop cue).
    fn in_start_secs(&self, incoming: usize, how: MixHow) -> f64 {
        let i = &self.snap.decks[incoming];
        let drop = (how == MixHow::Drop)
            .then(|| self.drop_cue(incoming))
            .flatten()
            .and_then(|c| i.hot_cues.get(c as usize).copied().flatten());
        drop.unwrap_or(if i.loop_on { i.loop_start_secs } else { i.position_secs })
    }

    /// The best transitions found by analysing both tracks (see [`crate::transition`]), best
    /// first; empty when a deck has no beat grid (then the fixed script plans the mix).
    pub fn search_plans(&self, incoming: usize, outgoing: usize, when: MixWhen, style: MixStyle) -> Vec<MixPlan> {
        use crate::transition::{self as tr, Limits, Technique};
        let (i, o) = (&self.snap.decks[incoming], &self.snap.decks[outgoing]);
        let (Some((inc, cb_i)), Some((out, _))) = (self.deck_bars(incoming), self.deck_bars(outgoing)) else { return Vec::new() };
        let start_i = inc.start;
        let stems = i.stems_ready && o.stems_ready && inc.stems.is_some() && out.stems.is_some();
        // Keys a semitone or two apart: the new track is transposed to fit, and they blend as
        // if they matched.
        let transpose = self.key_shift(incoming, outgoing).unwrap_or(0);
        let clash = self.keys_clash(incoming, outgoing) && transpose == 0;
        let stretch = (i.track_bpm / o.bpm.max(1.0) - 1.0).abs() > 0.08;
        // What the style allows: Auto blends unless the keys clash without stems or the tempos
        // are far apart; only then it cuts.
        let can_blend = !stretch && (stems || !clash);
        let (blend, cut) = match style {
            MixStyle::Auto => (can_blend, !can_blend),
            MixStyle::Stems | MixStyle::Eq => (true, false),
            MixStyle::Drop => (false, true),
        };
        let blend_how = if style == MixStyle::Eq || !stems { MixHow::Eq } else { MixHow::Stems };
        // Ahead: a paused deck needs a bar to start; its cue may need a few beats of lead-in.
        let lead = if i.playing { 0.5 } else { 4.5 } + 3.0;
        let earliest = ((o.beat + lead) / 4.0).ceil().max(0.0) as usize;
        let latest = if when == MixWhen::Now { earliest + 4 } else { out.len() };
        // Every phrase length (4 bars) from 8 up to 128: how long is up to the two songs (the
        // search scores each bar of the overlap where they sound right together).
        let lengths: Vec<usize> = (MIN_MIX_BARS / 4..=MAX_MIX_BARS / 4).map(|k| 4 * k).collect();
        // Your cue (or where a paused deck stands) as a bar of the new track. It's a preference
        // only when you chose it: a cue loop, or the deck cued away from where it loaded (its start
        // or first cue); otherwise dj picks the best place to bring it in.
        let cue_secs = if i.loop_on { i.loop_start_secs } else { i.position_secs };
        let cue_bar = ((cue_secs - start_i) / inc.bar_secs).round().max(0.0) as usize;
        let first_cue = i.hot_cues.iter().flatten().copied().fold(f64::MAX, f64::min);
        let chosen = i.loop_on || (!i.playing && cue_secs > 0.1 && (cue_secs - first_cue).abs() > 0.1);
        // The new vocals wait only for the old ones (see `plan_blend`), Now or Best.
        let vocals_out = false;
        let drop_secs = self.drop_cue(incoming).and_then(|c| i.hot_cues.get(c as usize).copied().flatten());
        let drop_cue_bar = drop_secs.map(|t| ((t - start_i) / inc.bar_secs).round().max(0.0) as usize);
        let lim = Limits {
            earliest,
            latest,
            lengths,
            cue_bar: chosen.then_some(cue_bar),
            drop_cue_bar,
            in_cues: cb_i.iter().filter(|b| **b >= -0.1).map(|b| b.round().max(0.0) as usize).collect(),
            blend,
            cut,
            keys_clash: clash,
            vocals_out,
            stems,
            hurry: when == MixWhen::Now,
        };
        tr::search(&out, &inc, &lim)
            .into_iter()
            .map(|c| {
                let how = match c.technique {
                    Technique::Cut => MixHow::Drop,
                    Technique::Blend => blend_how,
                };
                let bars = if how == MixHow::Drop { 1 } else { c.bars };
                let (entry_secs, lead_in) = if c.technique == Technique::Cut && Some(c.in_bar) == drop_cue_bar {
                    (drop_secs, self.lead_in(incoming, drop_secs.unwrap_or(cue_secs)))
                } else if c.in_bar == cue_bar {
                    (Some(cue_secs), self.lead_in(incoming, cue_secs))
                } else {
                    (Some(inc.secs(c.in_bar as f64)), 0.0)
                };
                let ob = |k: usize| c.out_bar + k;
                let inc_w: [Vec<f32>; 4] = std::array::from_fn(|p| (0..bars).map(|k| inc.level(p, c.in_bar + k)).collect());
                let out_w: [Vec<f32>; 4] = std::array::from_fn(|p| (0..bars).map(|k| out.level(p, ob(k))).collect());
                let mut blend = (how != MixHow::Drop).then(|| plan_blend(&inc_w, &out_w, bars, vocals_out, clash));
                // The old vocals end in a breath, not mid-line.
                if let (Some(b), Some(w)) = (blend.as_mut(), self.decks[outgoing].stem_wave.as_deref()) {
                    breathe_out(b, w, out.secs(c.out_bar as f64), out.bar_secs / 4.0);
                }
                // The new vocals start with a line, not in the middle of one.
                if let (Some(b), Some(w)) = (blend.as_mut(), self.decks[incoming].stem_wave.as_deref()) {
                    let spb_i = inc.bar_secs / 4.0;
                    breathe_in(b, w, entry_secs.unwrap_or(inc.secs(c.in_bar as f64)) + lead_in * spb_i, spb_i);
                }
                // Techniques, only when the music calls for them:
                // a high-pass sweep when the old track is still full as it has to go (EQ: no stems
                // to take it apart); an echo-out when it leaves mid-phrase with vocals, or is cut
                // while its bass plays.
                let last_full = (bars.saturating_sub(2)..bars).all(|k| out.fullness(ob(k)) >= 3);
                // (Done a bar before the end, like the EQ: the volume closes last.)
                let filter = (how == MixHow::Eq && bars >= 4 && last_full).then(|| ((bars - 3) as f64, (bars - 1) as f64));
                let mid_phrase = !(c.out_bar + bars).is_multiple_of(8);
                let echo_at = match how {
                    MixHow::Drop if out.on(tr::BASS, ob(0)) => Some(0.0),
                    MixHow::Eq | MixHow::Stems if mid_phrase && out.on(tr::VOCALS, ob(bars - 1)) => Some((bars - 1) as f64),
                    _ => None,
                };
                let mut notes = c.notes.clone();
                if transpose != 0 {
                    let key = |d: usize| self.deck_key(d).map(|k| k.camelot()).unwrap_or_default();
                    let to = self.deck_key(incoming).map(|k| k.transpose(transpose).camelot()).unwrap_or_default();
                    notes.insert(0, format!("new track {transpose:+} semitone{} ({} → {to}) to fit {}", if transpose.abs() > 1 { "s" } else { "" }, key(incoming), key(outgoing)));
                }
                if let Some(b) = &blend {
                    notes.extend(b.notes.iter().take(2).cloned());
                }
                if filter.is_some() {
                    notes.push("filter sweep out".into());
                }
                if echo_at.is_some() {
                    notes.push("echo out".into());
                }
                let reason = match how {
                    MixHow::Drop => "a cut".to_string(),
                    _ => format!("{bars} bars"),
                };
                let why_how = if style != MixStyle::Auto {
                    String::new()
                } else {
                    match how {
                        MixHow::Drop if stretch => "tempos far apart".into(),
                        MixHow::Drop if clash && !stems => "keys clash".into(),
                        MixHow::Drop => "it fits best".into(),
                        MixHow::Stems if clash => "keys clash: melodies switch at the swap".into(),
                        MixHow::Stems => "stems on both decks".into(),
                        MixHow::Eq => "no stems yet".into(),
                    }
                };
                MixPlan {
                    incoming,
                    outgoing,
                    mode: when,
                    how,
                    start_beat: c.out_bar as f64 * 4.0,
                    length_beats: bars as f64 * 4.0,
                    reason,
                    why_how,
                    blend,
                    lead_in,
                    entry_secs,
                    filter,
                    echo_at,
                    notes,
                    transpose,
                }
            })
            .collect()
    }

    /// A deck's track bar by bar (see [`crate::transition::Bars`]), with its phrases from its cue
    /// points, and those cues as bars. Bar 0 is its grid's beat 0 (outgoing bar k is mix beat 4k).
    pub fn deck_bars(&self, d: usize) -> Option<(crate::transition::Bars, Vec<f64>)> {
        let v = &self.snap.decks[d];
        let info = self.decks[d].info.as_ref()?;
        if !v.has_grid || v.track_bpm <= 0.0 {
            return None;
        }
        let spb = 60.0 / v.track_bpm;
        let start = v.position_secs - v.beat * spb;
        let cues: Vec<f64> = v.hot_cues.iter().flatten().map(|t| (t - start) / (4.0 * spb)).collect();
        // Mixed In Key's "Energy N" cues: the energy of each section.
        let energy: Vec<(f64, u8)> = info
            .cues
            .iter()
            .enumerate()
            .filter_map(|(i, c)| {
                let c = c.as_ref()?;
                let n: u8 = c.name.trim().strip_prefix("Energy")?.trim().parse().ok()?;
                let t = v.hot_cues.get(i).copied().flatten().unwrap_or(c.secs);
                Some(((t - start) / (4.0 * spb), n.clamp(1, 10)))
            })
            .collect();
        let bars = crate::transition::Bars::new(&info.waveform, self.decks[d].stem_wave.as_deref(), start, spb, v.duration_secs)
            .with_phrase(crate::transition::phrase_offset(&cues))
            .with_energy(&energy, self.deck_energy(d));
        let bars = match self.decks[d].sections.as_deref() {
            Some(sections) => bars.with_sections(sections),
            None => bars,
        };
        (!bars.is_empty()).then_some((bars, cues))
    }

    /// With two tracks loaded and nothing playing: which order mixes clearly better (the deck to
    /// play first, then the other), from the best transition each way.
    pub fn mix_order_hint(&self) -> Option<(usize, usize)> {
        use crate::transition::{self as tr, Limits};
        let decks: Vec<usize> = (0..self.deck_count()).filter(|&d| self.snap.decks[d].loaded && self.decks[d].info.is_some()).collect();
        if decks.len() != 2 || decks.iter().any(|&d| self.snap.decks[d].playing) {
            return None;
        }
        let best = |first: usize, then: usize| -> Option<f32> {
            let ((out, _), (inc, cb_i)) = (self.deck_bars(first)?, self.deck_bars(then)?);
            let stems = out.stems.is_some() && inc.stems.is_some();
            let lim = Limits {
                earliest: 0,
                latest: out.len(),
                lengths: (MIN_MIX_BARS / 4..=MAX_MIX_BARS / 4).map(|k| 4 * k).collect(),
                cue_bar: Some(0),
                drop_cue_bar: None,
                in_cues: cb_i.iter().filter(|b| **b >= -0.1).map(|b| b.round().max(0.0) as usize).collect(),
                blend: true,
                cut: false,
                keys_clash: self.keys_clash(then, first),
                vocals_out: false,
                stems,
                hurry: false,
            };
            tr::search(&out, &inc, &lim).first().map(|c| c.score)
        };
        let (a, b) = (decks[0], decks[1]);
        let (ab, ba) = (best(a, b)?, best(b, a)?);
        // Only a clear difference (a few bars' worth of fit).
        if (ab - ba).abs() < 3.0 {
            return None;
        }
        Some(if ab > ba { (a, b) } else { (b, a) })
    }

    /// Tell once per pair of tracks which order mixes better (see [`Core::mix_order_hint`]).
    pub(super) fn order_hint_tick(&mut self) {
        let ids: Vec<u64> = (0..self.deck_count()).filter(|&d| self.snap.decks[d].loaded).map(|d| self.snap.decks[d].track_id).collect();
        let ready = (0..self.deck_count()).filter(|&d| self.snap.decks[d].loaded).all(|d| self.snap.decks[d].stems_ready || self.decks[d].stems_missing.is_some());
        if ids.len() != 2 || !ready || self.order_hinted.as_deref() == Some(&ids[..]) {
            return;
        }
        self.order_hinted = Some(ids);
        if let Some((first, then)) = self.mix_order_hint() {
            let title = |d: usize| self.decks[d].info.as_ref().and_then(|i| i.tags.title.clone()).unwrap_or_else(|| format!("deck {}", d + 1));
            self.log(format!("Mix tip: play deck {} ({}) first, then deck {} ({}): they mix better that way", first + 1, title(first), then + 1, title(then)));
        }
    }

    /// Mix now: a blend from the next bar line over the whole bars that fit in about 10 s; a drop
    /// on the next 8-bar phrase start (or the next bar when the track ends sooner).
    pub fn plan_now(&self, incoming: usize, outgoing: usize, style: MixStyle) -> MixPlan {
        if let Some(p) = self.search_plans(incoming, outgoing, MixWhen::Now, style).into_iter().next() {
            return p;
        }
        let o = &self.snap.decks[outgoing];
        let (how, why_how) = self.resolve_style(style, incoming, outgoing, MixWhen::Now);
        // A paused deck needs a moment to start in sync: one bar more. And the new track may
        // start a few beats before the mix (its lead-in), which must still be ahead.
        let lead_in = self.lead_in(incoming, self.in_start_secs(incoming, how));
        let lead = if self.snap.decks[incoming].playing { 0.5 } else { 4.5 } + lead_in;
        // On a phrase of the outgoing track: its next 8-bar phrase when that's at most 4 bars
        // away, else the next 4-bar line (never more than 4 bars to wait).
        let phrase = ((o.beat + lead) / 32.0).ceil() * 32.0;
        let (next_bar, on) = if phrase - o.beat <= 16.0 {
            (phrase, "on the phrase")
        } else {
            (((o.beat + lead) / 16.0).ceil() * 16.0, "on the next 4 bars")
        };
        if how == MixHow::Drop {
            let (start_beat, reason) = (next_bar, on);
            return MixPlan { incoming, outgoing, mode: MixWhen::Now, how, start_beat, length_beats: DROP_BEATS, reason: reason.into(), why_how, blend: None, lead_in, ..MixPlan::plain() };
        }
        let bar_s = 4.0 * 60.0 / o.bpm.max(1.0);
        let bars = (NOW_SECS / bar_s).floor().clamp(8.0, 16.0);
        let blend = self.analyse(incoming, outgoing, next_bar, bars as usize, false);
        MixPlan { incoming, outgoing, mode: MixWhen::Now, how, start_beat: next_bar, length_beats: bars * 4.0, reason: format!("{bars:.0} bars, {on}"), why_how, blend, lead_in, ..MixPlan::plain() }
    }

    /// Mix at the best phrase start: where the vocals clash least over 16 bars, preferring the
    /// outgoing track's outro or break, then sooner over later.
    pub fn plan_best(&self, incoming: usize, outgoing: usize, style: MixStyle) -> MixPlan {
        if let Some(p) = self.search_plans(incoming, outgoing, MixWhen::Best, style).into_iter().next() {
            return p;
        }
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
                    let lead_in = self.lead_in(incoming, self.in_start_secs(incoming, how));
                    return MixPlan { incoming, outgoing, mode: MixWhen::Best, how, start_beat, length_beats: DROP_BEATS, reason, why_how, blend: None, lead_in, ..MixPlan::plain() };
                }
                // How long: from the energy step and the new track's intro.
                let (bars, why_len) = self.best_length(incoming, outgoing, start_beat, end_beat);
                let reason = if why_len.is_empty() { format!("{bars} bars · {reason}") } else { format!("{bars} bars ({why_len}) · {reason}") };
                let blend = self.analyse(incoming, outgoing, start_beat, bars, false);
                let lead_in = self.lead_in(incoming, self.in_start_secs(incoming, how));
                MixPlan { incoming, outgoing, mode: MixWhen::Best, how, start_beat, length_beats: bars as f64 * 4.0, reason, why_how, blend, lead_in, ..MixPlan::plain() }
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
        // (Its bars line up with the mix's: count them from its first downbeat.)
        let t_in = t_in + self.lead_in(incoming, t_in) * spb_i;
        let inc = bar_levels(wi, t_in, spb_i, bars);
        let out = bar_levels(wo, t_out, spb_o, bars);
        Some(plan_blend(&inc, &out, bars, vocals_out, self.keys_clash(incoming, outgoing)))
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
        } else if !self.snap.decks[i].loop_on {
            // Playing turned down: it waits in a loop where it is (silent), so it can't run out
            // before the mix.
            self.deck_cmd(i, DeckCmd::LoopBeats(self.loop_size[i]));
        }
        // Muted counts as silent: make it volume-down instead, so the fade-in is heard.
        if !external && self.session.channels[i].params.mute {
            self.set_channel(i, |p| {
                p.volume = 0.0;
                p.mute = false;
            });
        }
        let params = |c: usize| self.session.channels[c].params;
        // The new vocals stay out only if the old ones play through the whole mix; then they come
        // back on the new track's next vocal phrase.
        let vocals_out = self.snap.decks[i].stems_ready && plan.blend.as_ref().is_some_and(|b| b.in_at[VOCALS].is_some_and(|x| x >= b.bars as f64 - 0.5));
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
            out_cutoff: params(o).cutoff,
            echo_slot: None,
            in_semitones: self.snap.decks[i].semitones,
            plan,
        };
        // Its key fitted to the playing track's, while it's still silent.
        if mix.plan.transpose != 0 {
            self.deck_cmd(i, DeckCmd::Semitones(mix.in_semitones + mix.plan.transpose as f64));
        }
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
                    None => stems_at(0.0, vocals_out, true).0,
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
        // What dj found in the two tracks, in a few words (the full reasons go to the terminal).
        let found = mix.plan.notes.iter().find(|n| !n.starts_with("starts it") && !n.contains("on its own")).map(|n| format!(" · {n}")).unwrap_or_default();
        self.log(format!("Mixing deck {} into deck {} ({}): {when}, {} bars{found}", i + 1, o + 1, mix.plan.how.name(external), (mix.plan.length_beats / 4.0).round()));
        // Why this mix, for the terminal (the status line stays short).
        let p = &mix.plan;
        let why = [p.reason.as_str(), p.why_how.as_str()].into_iter().chain(p.notes.iter().map(String::as_str)).filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" · ");
        if !why.is_empty() {
            eprintln!("  {why}");
        }
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
            if !m.external && m.plan.filter.is_some() {
                self.mix_set(&format!("Mixer/CH{}/Filter1", o + 1), m.out_cutoff);
            }
            if let Some(slot) = m.echo_slot {
                self.remove_insert(o, slot);
            }
            if m.plan.transpose != 0 {
                self.deck_cmd(i, DeckCmd::Semitones(m.in_semitones));
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

    /// The incoming deck's next phrase (every 4 bars of its grid) where its vocals play, in its
    /// own beats; `None` if it has no more vocals (or no stems).
    fn next_vocal_phrase(&self, d: usize) -> Option<f64> {
        let w = self.decks[d].stem_wave.as_ref()?;
        let v = &self.snap.decks[d];
        let spb = 60.0 / v.track_bpm.max(1.0);
        let end = v.beat + (v.duration_secs - v.position_secs) / spb;
        let mut c = ((v.beat + 1.0) / 16.0).ceil() * 16.0;
        while c + 8.0 <= end {
            let t = v.position_secs + (c - v.beat) * spb;
            if bar_levels(w, t, spb, 2)[VOCALS].iter().any(|x| *x > PRESENT) {
                return Some(c);
            }
            c += 16.0;
        }
        None
    }

    fn vocal_return_tick(&mut self) {
        let Some(vr) = self.vocal_return else { return };
        let v = self.snap.decks[vr.deck];
        let clock = self.snap.clock.beat;
        let addr = format!("Deck/{}/StemVocalsLevel", vr.deck + 1);
        if v.track_id != vr.track_id || !v.loaded || (clock < vr.at_clock && v.stem_gains[VOCALS] > 0.01) {
            // Another track, or you brought them back yourself.
            self.vocal_return = None;
        } else if clock >= vr.at_clock {
            let x = (clock - vr.at_clock) / 4.0;
            self.set_picked_up(&addr, vr.level * smooth(x));
            if x >= 1.0 {
                self.vocal_return = None;
            }
        }
    }

    /// Ease the new track's tempo back to its own, once it leads the clock.
    fn tempo_home_tick(&mut self) {
        let Some(mut th) = self.tempo_home else { return };
        let v = self.snap.decks[th.deck];
        if v.track_id != th.track_id || !v.loaded || !self.settings.tempo_home {
            self.tempo_home = None;
            return;
        }
        // Until the handoff, it follows the old track's tempo; it leads once that one stops.
        if self.snap.clock.owner != ClockOwner::Deck(th.deck as u8) || !v.playing {
            return;
        }
        let clock = self.snap.clock.beat;
        let Some((fader, at, before)) = th.last else {
            th.last = Some((v.tempo_fader, clock, v.tempo_fader));
            self.tempo_home = Some(th);
            return;
        };
        if (v.tempo_fader - fader).abs() > 1e-6 && (v.tempo_fader - before).abs() > 1e-6 {
            // You moved it: yours from here.
            self.tempo_home = None;
            return;
        }
        let bars = ((clock - at) / 4.0).max(0.0);
        let speed = v.tempo_range.speed(fader);
        let step = TEMPO_HOME_PER_BAR * bars;
        let next = if (speed - 1.0).abs() <= step { 1.0 } else { speed - step * (speed - 1.0).signum() };
        let f = if next == 1.0 { 0.0 } else { v.tempo_range.fader_for_speed(next) };
        self.deck_cmd(th.deck, DeckCmd::Tempo(f));
        if f == 0.0 {
            self.tempo_home = None;
            self.log(format!("Deck {} is back at its own tempo", th.deck + 1));
        } else {
            th.last = Some((f, clock, fader));
            self.tempo_home = Some(th);
        }
    }

    /// Advance a running mix (called every tick).
    pub(super) fn automix_tick(&mut self) {
        self.vocal_return_tick();
        self.tempo_home_tick();
        self.order_hint_tick();
        let now = self.snap.clock.beat;
        let due: Vec<_> = self.echo_cleanup.iter().copied().filter(|e| now >= e.2).collect();
        self.echo_cleanup.retain(|e| now < e.2);
        for (ch, slot, _) in due {
            // Only if the slot still holds dj's echo (you may have changed it since).
            if self.session.channels[ch].fx.slots[slot].as_ref().is_some_and(|s| s.effect_id == ECHO_ID) {
                self.remove_insert(ch, slot);
            }
        }
        let Some(m) = self.automix.as_ref() else { return };
        let (i, o, start, len) = (m.plan.incoming, m.plan.outgoing, m.plan.start_beat, m.plan.length_beats);
        let (vi, vo) = (self.snap.decks[i], self.snap.decks[o]);
        // Another track loaded on either deck, or a deck stopped or ejected: give up (and put the
        // EQ and stems back). The old track reaching its end is fine: it fades on its own, and the
        // mix carries on bringing the new one in.
        let old_ended = vo.loaded && !vo.playing && vo.duration_secs > 0.0 && vo.position_secs >= vo.duration_secs - 0.5;
        if (vi.track_id, vo.track_id) != m.tracks || !vi.loaded || !vo.loaded || !(vo.playing || old_ended) || !(vi.playing || vi.play_pending) {
            self.cancel_mix();
            return;
        }
        // The outgoing track's beats, counted on the clock: a loop on that deck doesn't stall the mix.
        let beat = self.snap.clock.beat + m.beat_offset;
        // Half a beat ahead: the incoming deck leaves its loop on the next beat, which is `start`.
        // If it hasn't started playing yet (a paused deck waits for the beat), a bar later.
        let release = start - m.plan.lead_in;
        // Too late to start the new track on its beat (it starts on the next one): a bar later,
        // so the bars still line up. Same if it hasn't started playing yet (a paused deck).
        if !m.released && beat >= release - 0.05 {
            self.automix.as_mut().unwrap().plan.start_beat += 4.0;
            return;
        }
        if !m.released && beat >= release - 0.5 && !vi.playing {
            self.automix.as_mut().unwrap().plan.start_beat += 4.0;
            return;
        }
        if !m.released && beat >= release - 0.5 {
            // A drop starts from the incoming track's drop cue.
            let to_cue = if m.plan.how == MixHow::Drop && m.plan.entry_secs.is_none() { self.drop_cue(i) } else { None };
            let entry = m.plan.entry_secs;
            let external = m.external;
            self.automix.as_mut().unwrap().released = true;
            self.deck_cmd(i, DeckCmd::ReleaseLoop { to_cue, to_secs: entry });
            // Its volume fully up as it starts: its stems or EQ (still closed) bring it in.
            if !external {
                self.mix_set(&format!("Mixer/CH{}/Volume", i + 1), 1.0);
            }
            return;
        }
        if beat < start {
            return;
        }
        let p = (beat - start) / len;
        let (how, external, swapped) = (m.plan.how, m.external, m.swapped);
        let (out_volume, in_low, out_low, in_high, out_high, out_mid, in_mid) =
            (m.out_volume, m.in_low, m.out_low, m.in_high, m.out_high, m.out_mid, m.in_mid);
        // The new track's volume is up from the start; the EQ or stems do the blending.
        let blend = m.plan.blend.clone();
        let bp = p.min(1.0) * len / 4.0; // bar position in the mix
        // The old track's volume closes only over the last bar, once its EQ or stems have taken
        // it out.
        let bars_n = len / 4.0;
        let out_vol = 1.0 - smooth(bp - (bars_n - 1.0));
        let (filter, echo_at, echo_slot, out_cutoff) = (m.plan.filter, m.plan.echo_at, m.echo_slot, m.out_cutoff);
        let (in_stems, out_stems, vocals_out) = (m.in_stems, m.out_stems, m.vocals_out);
        let vol = |c: usize| format!("Mixer/CH{}/Volume", c + 1);
        let low = |c: usize| format!("Mixer/CH{}/EQ/Low", c + 1);
        let high = |c: usize| format!("Mixer/CH{}/EQ/High", c + 1);
        let mid = |c: usize| format!("Mixer/CH{}/EQ/Mid", c + 1);
        match how {
            MixHow::Eq if external => {} // you fade on the mixer
            MixHow::Eq => {
                // In (volume already up): the highs rise from the start, then the mids, the old
                // ones partly making room; the bass swap (where the plan puts it: where the new
                // bass comes in or the old one stops, else late on a phrase line). Out: mids, then
                // highs; its volume only closes the last part.
                // The old track's EQ is done a bar before the end (then its volume closes).
                let done = ((bars_n - 1.0) / bars_n).max(0.5);
                let (lo, hi) = swap_range(bars_n.round() as usize);
                let ps = blend.as_ref().map_or(0.5, |b| b.swap_fraction()).clamp(lo as f64 / bars_n, hi as f64 / bars_n);
                let after = (done - ps).max(0.05);
                // Each band over at most 8 bars (a long mix spreads them out, not slows them down).
                let span = |frac: f64| (frac * bars_n).min(8.0) / bars_n;
                if p < ps {
                    let (rise_h, rise_m) = (smooth(p / span(0.7 * ps)), smooth((p - 0.3 * ps) / span(0.7 * ps)));
                    self.mix_set(&high(i), in_high * rise_h);
                    self.mix_set(&mid(i), in_mid * rise_m);
                    // Room for them: the old highs and mids come partly down as the new ones rise,
                    // the old bass staying until the swap (how DJs stage it: highs and mids
                    // first, bass last).
                    self.mix_set(&high(o), out_high * (1.0 - 0.5 * rise_h));
                    self.mix_set(&mid(o), out_mid * (1.0 - 0.4 * rise_m));
                }
                if p >= ps && !swapped {
                    self.automix.as_mut().unwrap().swapped = true;
                    self.mix_set(&low(i), in_low);
                    self.mix_set(&mid(i), in_mid);
                    self.mix_set(&high(i), in_high);
                    self.mix_set(&low(o), 0.0);
                }
                if p >= ps {
                    // Out by `done` from where they were (60 % mids, half the highs): the mids
                    // first, then the highs, each over at most 8 bars.
                    let (m, h) = (span(0.6 * after), span(0.7 * after));
                    self.mix_set(&mid(o), 0.6 * out_mid * (1.0 - smooth((p - (ps + 0.6 * after - m)) / m)));
                    self.mix_set(&high(o), 0.5 * out_high * (1.0 - smooth((p - (done - h)) / h)));
                }
            }
            MixHow::Stems => {
                let (si, so) = match &blend {
                    Some(b) => b.levels(bp, vocals_out),
                    None => stems_at(p.min(1.0), vocals_out, true),
                };
                self.set_stem_levels(i, si, in_stems);
                self.set_stem_levels(o, so, out_stems);
                if !external {
                    // The outgoing track leaves on its stems; its volume stays up (it's at 0 at the end).
                    // The EQ smooths the stem swaps: the new highs rise until its drums are in, the
                    // old highs roll off as its drums leave (from the plan).
                    let (d_in, d_out) = match &blend {
                        Some(b) => (b.in_at[DRUMS].unwrap_or(b.swap).max(1.0), b.out_at[DRUMS].unwrap_or(b.bars as f64)),
                        None => (0.35 * len / 4.0, 0.75 * len / 4.0),
                    };
                    let ramp = d_in.min(8.0);
                    self.mix_set(&high(i), in_high * smooth((bp - (d_in - ramp)) / ramp));
                    self.mix_set(&high(o), out_high * (1.0 - smooth((bp - (d_out - 2.0)) / 2.0)));
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
        if !external && how != MixHow::Drop {
            self.mix_set(&vol(o), out_volume * out_vol);
            // A high-pass sweep on the old track where the plan puts one.
            if let Some((a, b)) = filter {
                let x = smooth((bp - a) / (b - a).max(0.5));
                self.mix_set(&format!("Mixer/CH{}/Filter1", o + 1), out_cutoff + (0.85 - out_cutoff) * x);
            }
        }
        // An echo-out: an echo after the old channel's fader, so its tail rings on.
        if let (Some(at), None, false) = (echo_at, echo_slot, external) {
            if bp >= at {
                // One try: a full chain gets no echo (and a note), not a retry every tick.
                self.automix.as_mut().unwrap().plan.echo_at = None;
                match self.add_insert(o, ECHO_ID, InsertPos::Post) {
                    Ok(slot) => self.automix.as_mut().unwrap().echo_slot = Some(slot),
                    Err(e) => self.log(format!("CH{}: no echo-out ({e})", o + 1)),
                }
            }
        }
        if p >= 1.0 {
            // Put the filter back; the echo goes two bars later (its tail).
            if !external && filter.is_some() {
                self.mix_set(&format!("Mixer/CH{}/Filter1", o + 1), out_cutoff);
            }
            if let Some(slot) = self.automix.as_ref().and_then(|m| m.echo_slot) {
                self.echo_cleanup.push((o, slot, self.snap.clock.beat + 8.0));
            }
            if external {
                // The old deck loops (silent with Stems or Drop) until you've faded it out: every
                // stem at 0, also those the plan left alone because they were silent in the mix.
                if how == MixHow::Stems && vo.stems_ready {
                    self.set_stem_levels(o, [0.0; 4], out_stems);
                }
                if old_ended {
                    self.log(format!("Deck {} is in, deck {} has ended", i + 1, o + 1));
                } else {
                    self.deck_cmd(o, DeckCmd::LoopBeats(self.loop_size[o]));
                    self.log(format!("Deck {} is in, deck {} loops until you fade it out", i + 1, o + 1));
                }
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
            // Its own tempo again, slowly, once it leads (it plays at the old track's).
            self.tempo_home = (self.settings.tempo_home && (vi.speed / vi.speed_mult.max(0.01) - 1.0).abs() > 0.002 && vi.has_grid)
                .then_some(TempoHome::new(i, vi.track_id));
            // The new track as it is from here: every stem in, except (Now) its vocals, which come
            // back on its next phrase that has vocals.
            if how == MixHow::Stems {
                self.set_stem_levels(i, [1.0, 1.0, if vocals_out { 0.0 } else { 1.0 }, 1.0], in_stems);
            }
            if vocals_out {
                self.vocal_return = self.next_vocal_phrase(i).map(|b| VocalReturn {
                    deck: i,
                    track_id: vi.track_id,
                    at_clock: self.snap.clock.beat + (b - vi.beat),
                    level: in_stems[VOCALS],
                });
            }
            self.automix = None;
        }
    }
}

/// A mix planned from the music: when each part of the two tracks comes in or leaves, from both
/// tracks' stem levels over the bars of the mix (see [`plan_blend`]).
#[derive(Clone, Debug, PartialEq)]
pub struct Blend {
    pub bars: usize,
    /// Mix bar where each incoming stem comes in (a one-bar ramp from there); `None` = left as the
    /// track plays (it's silent until it comes in on its own, or there from the start).
    pub in_at: [Option<f64>; 4],
    /// Bars each incoming stem takes to come up from its `in_at`.
    pub in_ramp: [f64; 4],
    /// Mix bar by which each outgoing stem has left (a ramp over `out_ramp` bars before it);
    /// `None` = left alone (it stops on its own, or isn't there).
    pub out_at: [Option<f64>; 4],
    pub out_ramp: [f64; 4],
    /// Bar where the basses swap.
    pub swap: f64,
    /// What the plan follows ("bass swaps at bar 9, where its bass comes in").
    pub notes: Vec<String>,
}

impl Blend {
    /// Stem levels (fractions of the decks' own) at bar position `bp` of the mix: (in, out).
    pub fn levels(&self, bp: f64, vocals_out: bool) -> ([f32; 4], [f32; 4]) {
        let mut i = [1.0f32; 4];
        let mut o = [1.0f32; 4];
        for s in 0..4 {
            if let Some(b) = self.in_at[s] {
                i[s] = if s == BASS { (bp >= b) as u8 as f32 } else { smooth((bp - b) / self.in_ramp[s].max(0.1)) };
            }
            if let Some(b) = self.out_at[s] {
                o[s] = if s == BASS { (bp < b) as u8 as f32 } else { 1.0 - smooth((bp - (b - self.out_ramp[s])) / self.out_ramp[s].max(0.1)) };
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

/// Move the old vocals' fade into a pause of the singer's: the vocals stay at full level until
/// the nearest beat (within a bar or two of the planned fade) where the vocal stem is quiet, and
/// fade out inside it. `start` is the old track's time at the mix's first bar, `spb` its seconds
/// per beat. Unchanged when the old vocals leave on their own or no pause is near.
pub fn breathe_out(b: &mut Blend, w: &crate::stems::StemWave, start: f64, spb: f64) {
    let Some(end) = b.out_at[VOCALS] else { return };
    let v = &w.levels[VOCALS];
    let level = |beat: f64| {
        let (a, z) = (((start + beat * spb) * w.bins_per_sec) as usize, ((start + (beat + 1.0) * spb) * w.bins_per_sec) as usize);
        let (a, z) = (a.min(v.len()), z.min(v.len()));
        if z > a { v[a..z].iter().sum::<f32>() / (z - a) as f32 } else { 0.0 }
    };
    // How loud the singing is around there (the louder half of the beats).
    let from = ((end - b.out_ramp[VOCALS]) * 4.0 - 8.0).max(0.0);
    let to = (end * 4.0 + 4.0).min(b.bars as f64 * 4.0 - 4.0);
    if to <= from {
        return;
    }
    let mut around: Vec<f32> = (from as usize..to as usize).map(|k| level(k as f64)).collect();
    around.sort_by(|a, z| z.total_cmp(a));
    let sung = around.get(around.len() / 4).copied().unwrap_or(0.0);
    if sung <= PRESENT {
        return; // hardly singing there: nothing to cut
    }
    // The pause nearest the planned end of the fade.
    let target = end * 4.0 - 1.0;
    let gap = (from as usize..to as usize)
        .map(|k| k as f64)
        .filter(|k| level(*k) < 0.25 * sung)
        .min_by(|a, z| (a - target).abs().total_cmp(&(z - target).abs()));
    if let Some(g) = gap {
        b.out_at[VOCALS] = Some((g + 1.0) / 4.0);
        b.out_ramp[VOCALS] = 0.25;
        b.notes.push(format!("old vocals end in a pause (bar {})", (g / 4.0) as usize + 1));
    }
}

/// Bring the new vocals in at the start of a line: from where the plan lets them in, the next
/// beat (within 2 bars) where the vocal stem starts singing after a pause; they fade in during
/// that pause, so the first word is there. `start` is the new track's time at the mix's first
/// bar, `spb` its seconds per beat. Unchanged when no line starts that soon.
pub fn breathe_in(b: &mut Blend, w: &crate::stems::StemWave, start: f64, spb: f64) {
    let Some(at) = b.in_at[VOCALS] else { return };
    let v = &w.levels[VOCALS];
    let level = |beat: f64| {
        let (a, z) = (((start + beat * spb).max(0.0) * w.bins_per_sec) as usize, ((start + (beat + 1.0) * spb).max(0.0) * w.bins_per_sec) as usize);
        let (a, z) = (a.min(v.len()), z.min(v.len()));
        if z > a { v[a..z].iter().sum::<f32>() / (z - a) as f32 } else { 0.0 }
    };
    let total = b.bars as f64 * 4.0;
    let from = (at * 4.0).max(0.0);
    // How loud the singing is from there (the louder quarter of the beats).
    let mut ahead: Vec<f32> = (from as usize..(from + 32.0).min(total) as usize).map(|k| level(k as f64)).collect();
    if ahead.is_empty() {
        return;
    }
    ahead.sort_by(|a, z| z.total_cmp(a));
    let sung = ahead[ahead.len() / 4];
    if sung <= PRESENT {
        return; // no singing there to align
    }
    let quiet = |k: f64| level(k) < 0.25 * sung;
    // A line starts on beat k: singing there, a pause just before (never earlier than planned).
    let line = (from as usize + 1..=(from + 8.0).min(total - 1.0) as usize).map(|k| k as f64).find(|k| !quiet(*k) && quiet(k - 1.0));
    if let Some(k) = line {
        b.in_at[VOCALS] = Some((k - 1.0) / 4.0);
        b.in_ramp[VOCALS] = 0.25;
        b.notes.push(format!("new vocals start with a line (bar {})", (k / 4.0) as usize + 1));
    }
}

/// The bars the bass swap can be on, in a mix of `bars` bars.
pub fn swap_range(bars: usize) -> (usize, usize) {
    let lo = ((bars as f64 * 0.2).ceil() as usize).max(1);
    (lo, bars.saturating_sub(3).max(lo))
}

/// Plan which part of each track moves when, from their per-bar stem levels over the mix
/// (`inc` and `out`, `bars` long; see [`bar_levels`]):
///
/// - The basses swap where the new bass comes in on its own, else where the old bass stops on
///   its own, else on the phrase line nearest 70 % of the way: real DJs take the old bass out
///   late, as the last part of the old track to go (the DJ-mix studies' EQ curves).
/// - New drums that are there from the start come in halfway to the swap (on a phrase line);
///   drums that enter on
///   their own are left alone. Same for the melody (other), which is always let in.
/// - New vocals (Best) wait until the old vocals are gone.
/// - Old drums leave after the swap, halfway to the end; old vocals and other by the end; any
///   part that stops (or isn't there) on its own is left alone.
///
/// When the keys clash (`clash`), only drums and bass overlap: the old melody and vocals leave
/// before the swap and the new ones come in after it.
pub fn plan_blend(inc: &[Vec<f32>; 4], out: &[Vec<f32>; 4], bars: usize, vocals_out: bool, clash: bool) -> Blend {
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

    // The bass swap: where one bass keeps playing in the most bars (the old one until the swap,
    // the new one after it); of those, where the new bass comes in or the old one stops on its
    // own, then on a phrase line (every 4 bars into the mix), then the one nearest 70 % in.
    let (e_in, e_out) = (enters(&inc[BASS]), stops(&out[BASS]));
    let with_bass = |sw: usize| (0..bars).filter(|&k| if k < sw { on(&out[BASS], k) } else { on(&inc[BASS], k) }).count();
    let natural = |sw: usize| Some(sw) == e_in || Some(sw) == e_out;
    let late = (bars as f64 * 0.7).round() as usize;
    let phrase = |k: usize| bars >= 8 && k.is_multiple_of(4);
    // (From a fifth of the way in, so the new track has come up, to 3 bars before the end, so the
    // old one can be taken out before its volume closes; the EQ mix keeps to the same range.)
    let (lo, hi) = swap_range(bars);
    // No old bass to swap with at the start: the new bass may come in from the first bars.
    let lo = if !on(&out[BASS], 0) && !on(&out[BASS], 1) { 1 } else { lo };
    let sw = (lo..=hi).max_by_key(|&sw| (with_bass(sw), natural(sw), phrase(sw), usize::MAX - sw.abs_diff(late))).unwrap_or(late.clamp(lo, hi));
    notes.push(if Some(sw) == e_out {
        format!("bass swaps at bar {}, where the old bass stops", sw + 1)
    } else if Some(sw) == e_in {
        format!("bass swaps at bar {}, where the new bass comes in", sw + 1)
    } else if with_bass(sw) > with_bass(late.clamp(lo, hi)) {
        format!("bass swaps at bar {}, keeping a bass playing", sw + 1)
    } else {
        format!("bass swaps at bar {}, late on a phrase line (the old bass goes last)", sw + 1)
    });
    let swap = sw as f64;

    let mut in_at = [None; 4];
    let mut in_ramp = [1.0; 4];
    let mut out_at = [None; 4];
    in_at[BASS] = Some(swap);
    out_at[BASS] = Some(swap);
    let r = if bars >= 8 { 2.0 } else { 1.0 };
    let mut out_ramp = [r; 4];
    // A gentle lead: the new melody (and vocals, when the old track has none) there from the start
    // rises from nothing over a phrase or two (at most 8 bars, by the swap).
    let lead = (0.75 * swap).clamp(1.0, 8.0);
    for s in [OTHER, VOCALS] {
        if on(&inc[s], 0) {
            in_at[s] = Some(0.0);
            in_ramp[s] = lead;
        }
    }
    // New drums: there from the start → they come up from halfway to the swap, by the swap; with
    // no old drums to compete with, right away.
    if on(&inc[DRUMS], 0) && !on(&out[DRUMS], 0) && !on(&out[DRUMS], 1) {
        in_at[DRUMS] = Some(0.0);
        in_ramp[DRUMS] = r;
        notes.push("new drums come in at once (no old drums)".into());
    } else if on(&inc[DRUMS], 0) {
        // (On the phrase line before halfway to the swap, when there is one.)
        let half_way = (swap / 2.0).floor();
        let at = if half_way >= 4.0 { half_way - half_way % 4.0 } else { half_way }.max(1.0).min(swap);
        in_at[DRUMS] = Some(at);
        in_ramp[DRUMS] = (swap - at).clamp(1.0, 4.0);
    } else if enters(&inc[DRUMS]).is_some() {
        notes.push("new drums come in on their own".into());
    }
    // New vocals (Best): after the old ones.
    if !vocals_out && present(&inc[VOCALS]) && present(&out[VOCALS]) {
        let old_last = (0..bars).rev().find(|&k| on(&out[VOCALS], k)).unwrap_or(0);
        let wait = (old_last + 1).min(bars) as f64;
        in_at[VOCALS] = Some(wait);
        in_ramp[VOCALS] = r;
        notes.push(format!("new vocals wait for bar {}", wait as usize + 1));
    }
    // The old track is out on its stems a bar before the end (its volume closes over that bar).
    let done = (bars as f64 - 1.0).max(swap);
    let rest = done - swap;
    // Old drums: after the swap, unless they stop on their own; and only where the new drums
    // play, so the beat never drops out (a gap in the new intro keeps the old drums a while).
    if present(&out[DRUMS]) && stops(&out[DRUMS]).is_none_or(|x| x as f64 > swap + rest / 2.0) {
        let planned = (swap + (rest / 2.0).ceil()).min(done);
        let covered = |t: f64| {
            let (a, b) = ((t - r).max(0.0) as usize, (t as usize + 1).min(bars - 1));
            (a..=b).all(|k| on(&inc[DRUMS], k))
        };
        let mut t = planned;
        while t < done && !covered(t) {
            t += 1.0;
        }
        if t > planned {
            notes.push(if covered(t) { format!("old drums stay until bar {}, where the new drums play", t as usize + 1) } else { "old drums stay to the end: the new track has none yet".into() });
        }
        out_at[DRUMS] = Some(t);
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
            Some(x) if x as f64 <= done => {
                if s == VOCALS {
                    notes.push(format!("old vocals end at bar {}", x + 1));
                }
            }
            // Fading after the swap until they're out (over a phrase or two).
            _ => {
                out_at[s] = Some(done);
                out_ramp[s] = rest.clamp(r, 8.0);
            }
        }
    }
    if clash {
        // Keys clash: the melodies and vocals never play together; they switch at the swap.
        in_at[OTHER] = Some(swap);
        in_at[VOCALS] = Some(in_at[VOCALS].unwrap_or(swap).max(swap));
        in_ramp[OTHER] = r;
        in_ramp[VOCALS] = r;
        out_at[OTHER] = Some(swap);
        out_at[VOCALS] = Some(swap);
        out_ramp[OTHER] = r;
        out_ramp[VOCALS] = r;
        notes.insert(1.min(notes.len()), format!("keys clash: melodies switch at bar {}", swap as usize + 1));
    }
    Blend { bars, in_at, in_ramp, out_at, out_ramp, swap, notes }
}

#[cfg(test)]
mod tests {
    use super::{breathe_in, breathe_out, plan_blend, smooth, stems_at, BASS, DRUMS, OTHER, VOCALS};

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
        assert_eq!((i, o), ([0.0; 4], [1.0; 4]), "start: nothing of the new track yet");
        assert!(stems_at(0.2, true, false).0[3] > 0.0, "its melody (other) rises first");
        assert_eq!(stems_at(0.45, true, false).0[0], 1.0, "then its drums");
        let (i, o) = stems_at(0.5, true, false);
        assert_eq!((i[1], o[1]), (1.0, 0.0), "midpoint: the basses swap");
        assert_eq!(o[0], 1.0);
        let (_, o) = stems_at(0.65, true, false);
        assert!(o[0] == 0.0 && o[2] > 0.0 && o[3] > 0.0, "outgoing drums gone, then vocals and other");
        assert_eq!(stems_at(0.75, true, false).1, [0.0; 4], "all out before its volume closes");
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
        let b = plan_blend(&inc, &out, 16, true, false);
        assert_eq!(b.swap, 8.0);
        assert!(b.notes[0].contains("new bass comes in"), "{:?}", b.notes);
        // The old bass stops at bar 7 (the new one is there from the start): swap there.
        let b = plan_blend(&lv([ALL, ALL, NONE, ALL]), &lv([ALL, &[(0, 6)], ALL, ALL]), 16, true, false);
        assert_eq!(b.swap, 6.0);
        assert!(b.notes[0].contains("old bass stops"));
        // Neither: late, on a phrase line (12 bars into 16: the old bass goes last, as real DJs
        // take it out).
        let b = plan_blend(&lv([ALL, ALL, ALL, ALL]), &lv([ALL, ALL, ALL, ALL]), 16, true, false);
        assert_eq!(b.swap, 12.0);
        assert!(b.notes[0].contains("late on a phrase line"), "{:?}", b.notes);
        let full32: [Vec<f32>; 4] = std::array::from_fn(|_| vec![0.5; 32]);
        assert_eq!(plan_blend(&full32, &full32, 32, true, false).swap, 24.0);
    }

    #[test]
    fn parts_that_move_on_their_own_are_left_alone() {
        // New drums only from bar 4, old drums stop at bar 5, no old vocals.
        let inc = lv([&[(3, 16)], ALL, NONE, ALL]);
        let out = lv([&[(0, 5)], ALL, NONE, ALL]);
        let b = plan_blend(&inc, &out, 16, true, false);
        assert_eq!(b.in_at[DRUMS], None, "new drums come in by themselves");
        assert_eq!(b.out_at[DRUMS], None, "old drums leave by themselves");
        assert_eq!(b.out_at[VOCALS], None);
        assert!(b.notes.iter().any(|n| n.contains("no old vocals")), "{:?}", b.notes);
        // Drums there from the start are brought in halfway to the swap; old drums leave after it.
        let b = plan_blend(&lv([ALL, ALL, NONE, ALL]), &lv([ALL, ALL, NONE, ALL]), 16, true, false);
        assert_eq!(b.swap, 12.0);
        assert_eq!(b.in_at[DRUMS], Some(4.0), "on the phrase line before halfway to the swap");
        assert_eq!(b.out_at[DRUMS], Some(14.0));
        assert_eq!(b.out_at[OTHER], Some(15.0), "out a bar before the end (then the volume closes)");
        assert_eq!(b.in_at[OTHER], Some(0.0), "the new melody rises from the start");
        assert!(b.levels(0.0, true).0[OTHER] == 0.0 && b.levels(3.0, true).0[OTHER] < 1.0, "gently");
    }

    #[test]
    fn new_vocals_wait_for_the_old_ones() {
        let inc = lv([ALL, ALL, ALL, ALL]);
        let out = lv([ALL, ALL, &[(0, 11)], ALL]);
        let b = plan_blend(&inc, &out, 16, false, false);
        assert_eq!(b.in_at[VOCALS], Some(11.0));
        assert_eq!(b.out_at[VOCALS], None, "old vocals end on their own at bar 12");
        // Levels follow the plan: bass is a clean switch at the swap; others ramp.
        assert_eq!(b.swap, 12.0);
        let (i, o) = b.levels(11.9, false);
        assert_eq!((i[BASS], o[BASS]), (0.0, 1.0));
        let (i, o) = b.levels(12.0, false);
        assert_eq!((i[BASS], o[BASS]), (1.0, 0.0));
        assert_eq!(b.levels(10.0, false).0[VOCALS], 0.0);
        assert_eq!(b.levels(13.0, false).0[VOCALS], 1.0);
        assert_eq!(b.levels(12.0, true).0[VOCALS], 0.0, "Now: vocals stay out");
    }

    #[test]
    fn clashing_keys_only_overlap_drums_and_bass() {
        let inc = lv([ALL, ALL, ALL, ALL]);
        let out = lv([ALL, ALL, ALL, ALL]);
        let b = plan_blend(&inc, &out, 16, false, true);
        assert!(b.notes.iter().any(|n| n.contains("keys clash")), "{:?}", b.notes);
        // At no point do both melodies (or both vocals) play.
        for q in 0..64 {
            let bp = q as f64 / 4.0;
            let (i, o) = b.levels(bp, false);
            assert!(i[OTHER] * o[OTHER] < 1e-6, "melodies overlap at bar {bp}: {} {}", i[OTHER], o[OTHER]);
            assert!(i[VOCALS] * o[VOCALS] < 1e-6, "vocals overlap at bar {bp}");
        }
        // Drums and bass still blend: both decks' drums play together for a while.
        assert!((0..64).any(|q| {
            let (i, o) = b.levels(q as f64 / 4.0, false);
            i[DRUMS] > 0.5 && o[DRUMS] > 0.5
        }));
    }

    /// Per-bar levels for `n` bars: (from, to) bar ranges where each stem plays.
    fn lvn(n: usize, ranges: [&[(usize, usize)]; 4]) -> [Vec<f32>; 4] {
        std::array::from_fn(|st| (0..n).map(|k| if ranges[st].iter().any(|&(a, b)| (a..b).contains(&k)) { 0.5 } else { 0.0 }).collect())
    }

    #[test]
    fn the_beat_never_drops_out_and_parts_move_over_phrases() {
        // Push's intro under Too Good's last stretch: new drums with a gap at bars 13-16, its bass
        // from bar 5 with a pause at 13-24; the old bass stops at bar 10.
        let inc = lvn(40, [&[(0, 12), (16, 40)], &[(4, 12), (24, 40)], &[(12, 40)], &[(0, 40)]]);
        let out = lvn(40, [&[(0, 40)], &[(0, 9)], &[(0, 23)], &[(0, 40)]]);
        let b = plan_blend(&inc, &out, 40, false, false);
        // The swap keeps a bass playing: the old one until it stops (bar 10), not at the new one's
        // first entry (bar 5).
        assert_eq!(b.swap, 9.0, "{:?}", b.notes);
        // The old drums leave only where the new drums play (not in their gap).
        let t = b.out_at[DRUMS].unwrap();
        let (a, z) = ((t - b.out_ramp[DRUMS]) as usize, t as usize);
        for (k, v) in inc[DRUMS].iter().enumerate().take(z + 1).skip(a) {
            assert!(*v > 0.0, "old drums leave at bar {k} while the new ones are out");
        }
        // Every ramp is a phrase or two, not most of the mix.
        for r in b.in_ramp.iter().chain(b.out_ramp.iter()) {
            assert!(*r <= 8.0, "{b:?}");
        }
        assert!(b.levels(8.0, false).0[OTHER] >= 0.99, "the new melody is in within 8 bars");
    }

    #[test]
    fn parts_come_in_when_nothing_competes() {
        // The old track's outro: melody only (no drums, bass or vocals). The new track's intro
        // has drums, bass and vocals from the start.
        let out = lvn(24, [&[], &[], &[], &[(0, 24)]]);
        let inc = lvn(24, [&[(0, 24)], &[(0, 24)], &[(0, 24)], &[(0, 24)]]);
        let b = plan_blend(&inc, &out, 24, false, false);
        assert_eq!(b.in_at[DRUMS], Some(0.0), "drums at once: {:?}", b.notes);
        assert!(b.swap <= 2.0, "the bass right away: {}", b.swap);
        assert!(b.levels(8.0, false).0[VOCALS] >= 0.99, "vocals in (no old vocals)");
        // With old vocals through the whole mix, the new ones wait to the end.
        let sung = lvn(24, [&[], &[], &[(0, 24)], &[(0, 24)]]);
        let b = plan_blend(&inc, &sung, 24, false, false);
        assert!(b.in_at[VOCALS].unwrap() >= 23.5);
    }

    #[test]
    fn old_vocals_end_in_a_pause() {
        // 16 bars at 120 BPM (0.5 s a beat); the singer pauses on the last beat of every 2 bars.
        let bins_per_sec = 100.0;
        let v: Vec<f32> = (0..(32.0 * bins_per_sec) as usize).map(|i| if (i as f64 / bins_per_sec / 0.5) as usize % 8 == 7 { 0.01 } else { 0.6 }).collect();
        let w = crate::stems::StemWave { bins_per_sec, levels: [vec![0.5; v.len()], vec![0.5; v.len()], v, vec![0.5; 3200]] };
        let all = lvn(16, [&[(0, 16)], &[(0, 16)], &[(0, 16)], &[(0, 16)]]);
        let mut b = plan_blend(&all, &all, 16, false, false);
        b.out_at[VOCALS] = Some(13.0);
        b.out_ramp[VOCALS] = 4.0;
        breathe_out(&mut b, &w, 0.0, 0.5);
        let end = b.out_at[VOCALS].unwrap();
        // Out at the end of a pause beat (beats 7, 15, … end at bars 2, 4, …), the nearest to 13.
        assert_eq!(end, 12.0, "{:?}", b.notes);
        assert_eq!(b.out_ramp[VOCALS], 0.25);
        assert!(b.levels(11.7, false).1[VOCALS] >= 0.99, "full until the pause");
        assert_eq!(b.levels(12.0, false).1[VOCALS], 0.0);
    }

    #[test]
    fn new_vocals_start_with_a_line() {
        // 16 bars at 120 BPM; the new singer pauses on the last beat of every 2 bars, so lines
        // start on beats 8, 16, 24, …
        let bins_per_sec = 100.0;
        let v: Vec<f32> = (0..(32.0 * bins_per_sec) as usize).map(|i| if (i as f64 / bins_per_sec / 0.5) as usize % 8 == 7 { 0.01 } else { 0.6 }).collect();
        let w = crate::stems::StemWave { bins_per_sec, levels: [vec![0.5; v.len()], vec![0.5; v.len()], v, vec![0.5; 3200]] };
        let all = lvn(16, [&[(0, 16)], &[(0, 16)], &[(0, 16)], &[(0, 16)]]);
        let mut b = plan_blend(&all, &all, 16, false, false);
        // Planned in at bar 4.6 (beat 18.4): mid-line; the next line starts on beat 24 (bar 7).
        b.in_at[VOCALS] = Some(4.6);
        b.in_ramp[VOCALS] = 2.0;
        breathe_in(&mut b, &w, 0.0, 0.5);
        assert_eq!(b.in_at[VOCALS], Some(5.75), "{:?}", b.notes);
        assert!(b.levels(5.7, false).0[VOCALS] < 0.01, "not yet");
        assert!(b.levels(6.0, false).0[VOCALS] >= 0.99, "the first word at full level");
    }
}
