//! Finding the best transition between two tracks from what they play, bar by bar.
//!
//! [`Bars`] holds a track's analysis: per bar, the level of each stem (when separated) and of the
//! low/mid/high bands and loudness (always, from the waveform), and from those its sections:
//! where the bass plays, its drops (bass back after 4+ bars without), its intro and outro.
//! [`search`] tries every combination of where the old track can start leaving (phrase lines),
//! where the new track can come in (its cue, its start, the phrases before its drops, its drops)
//! and how long the overlap lasts, scores each on what would be heard (clashes, fullness, how the
//! sections fit, a drop landing as the old track ends, phrase alignment, waiting), and returns the
//! best ones. The new track's volume is up from the start and its stems or EQ bring it in; the
//! old one leaves on its stems or EQ, its volume closing only at the end.

/// Stems in this order: drums, bass, vocals, other.
pub const DRUMS: usize = 0;
pub const BASS: usize = 1;
pub const VOCALS: usize = 2;
pub const OTHER: usize = 3;

/// A part counts as playing in a bar above this level (relative to the track's peak).
pub const PRESENT: f32 = 0.08;

/// A track, bar by bar. Bar 0 starts at the grid's beat 0. (Kept on disk in a track's mix
/// profile, without what is worked out from the rest: see `crate::profile`.)
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Bars {
    /// Track time of bar 0 and the length of a bar, in seconds.
    pub start: f64,
    pub bar_secs: f64,
    /// Per stem (when separated): mean level per bar.
    pub stems: Option<[Vec<f32>; 4]>,
    /// Low, mid, high band and peak loudness per bar (from the waveform).
    pub bands: [Vec<f32>; 3],
    pub loud: Vec<f32>,
    /// The bar (0–7) the track's 8-bar phrases start on (see [`phrase_offset`]).
    pub phrase: usize,
    /// Energy (1–10) per bar, when known (see [`Bars::with_energy`]).
    pub energy: Vec<f32>,
    /// The song's sections from All-In-One, when found (see [`Bars::with_sections`]): the label
    /// of each bar ("intro", "verse", "chorus", "break", "outro"…), and the bars where one starts.
    #[serde(skip)]
    pub labels: Vec<&'static str>,
    #[serde(skip)]
    pub section_starts: Vec<usize>,
    /// Sections, worked out once (see [`Bars::finished`]).
    #[serde(skip)]
    pub(crate) drop_bars: Vec<usize>,
    #[serde(skip)]
    pub(crate) intro: usize,
    #[serde(skip)]
    pub(crate) outro: usize,
}

impl Bars {
    /// Mean of `v` (sampled `per_sec` times a second) over each bar of a track.
    fn per_bar(v: &[f32], per_sec: f64, start: f64, bar_secs: f64, n: usize) -> Vec<f32> {
        (0..n)
            .map(|k| {
                let (a, b) = (start + k as f64 * bar_secs, start + (k + 1) as f64 * bar_secs);
                let (a, b) = (((a.max(0.0)) * per_sec) as usize, ((b.max(0.0)) * per_sec) as usize);
                let (a, b) = (a.min(v.len()), b.min(v.len()));
                if b > a {
                    v[a..b].iter().sum::<f32>() / (b - a) as f32
                } else {
                    0.0
                }
            })
            .collect()
    }

    /// From the waveform (always) and the stems (if any). `start` = track time of grid beat 0.
    pub fn new(wave: &dj_library::Waveform, stems: Option<&crate::stems::StemWave>, start: f64, beat_secs: f64, duration: f64) -> Self {
        let bar_secs = 4.0 * beat_secs;
        let n = (((duration - start) / bar_secs).floor().max(0.0)) as usize;
        let pb = |v: &[f32], per: f64| Self::per_bar(v, per, start, bar_secs, n);
        Bars {
            start,
            bar_secs,
            stems: stems.map(|w| std::array::from_fn(|s| pb(&w.levels[s], w.bins_per_sec))),
            bands: [pb(&wave.low, wave.bins_per_sec), pb(&wave.mid, wave.bins_per_sec), pb(&wave.high, wave.bins_per_sec)],
            loud: pb(&wave.peak, wave.bins_per_sec),
            ..Default::default()
        }
        .finished()
    }

    /// Work out the sections (drops, intro, outro) once the levels are in.
    pub fn finished(mut self) -> Self {
        let n = self.len();
        self.drop_bars = (4..n).filter(|&k| self.on(BASS, k) && (k - 4..k).all(|j| !self.on(BASS, j))).collect();
        self.intro = (0..n).find(|&k| self.on(BASS, k)).unwrap_or(0);
        self.outro = (0..n).rev().find(|&k| self.on(BASS, k)).map_or(0, |k| k + 1);
        self
    }

    pub fn len(&self) -> usize {
        self.loud.len()
    }

    pub fn is_empty(&self) -> bool {
        self.loud.is_empty()
    }

    /// Track time where bar `k` starts.
    pub fn secs(&self, k: f64) -> f64 {
        self.start + k * self.bar_secs
    }

    /// Level of a part in bar `k`: the stem, or without stems an estimate from the bands
    /// (bass ≈ low band; drums ≈ low and high; vocals and other ≈ mid).
    pub fn level(&self, part: usize, k: usize) -> f32 {
        match &self.stems {
            Some(s) => s[part].get(k).copied().unwrap_or(0.0),
            None => {
                let b = |i: usize| self.bands[i].get(k).copied().unwrap_or(0.0);
                match part {
                    BASS => b(0),
                    DRUMS => 0.5 * (b(0) + b(2)),
                    _ => b(1),
                }
            }
        }
    }

    /// How much of the track has vocals: the share of its bars with music in which they sing
    /// (0–100 %). `None` without stems.
    pub fn vocal_share(&self) -> Option<u8> {
        self.stems.as_ref()?;
        // (Bars with music: within 14 dB of the loudest.)
        let loudest = self.loud.iter().copied().fold(0.0f32, f32::max);
        let playing: Vec<usize> = (0..self.len()).filter(|&k| self.loud[k] > 0.2 * loudest && loudest > 0.0).collect();
        let sung = playing.iter().filter(|&&k| self.on(VOCALS, k)).count();
        (!playing.is_empty()).then(|| (100.0 * sung as f64 / playing.len() as f64).round() as u8)
    }

    pub fn on(&self, part: usize, k: usize) -> bool {
        k < self.len() && self.level(part, k) > PRESENT
    }

    /// How many parts play in bar `k`.
    pub fn fullness(&self, k: usize) -> usize {
        (0..4).filter(|&p| self.on(p, k)).count()
    }

    /// Drops: bars where the bass comes back after 4+ bars without it (after an intro, break or
    /// build).
    pub fn drops(&self) -> &[usize] {
        &self.drop_bars
    }

    /// First bar where the bass plays (the end of the intro).
    pub fn intro_end(&self) -> usize {
        self.intro
    }

    /// First bar after the last bar with bass (the outro).
    pub fn outro_start(&self) -> usize {
        self.outro
    }

    /// With the phrases starting on bar `phrase` (from its cues).
    pub fn with_phrase(mut self, phrase: usize) -> Self {
        self.phrase = phrase % 8;
        self
    }

    /// Energy per bar: from Mixed In Key's "Energy N" cues (`cues`: bar and level; each holds
    /// until the next), else the track's overall energy shaped by its loudness.
    pub fn with_energy(mut self, cues: &[(f64, u8)], overall: Option<u8>) -> Self {
        let n = self.len();
        let mut cues: Vec<(f64, u8)> = cues.to_vec();
        cues.sort_by(|a, b| a.0.total_cmp(&b.0));
        self.energy = if let Some(first) = cues.first() {
            (0..n).map(|k| cues.iter().rev().find(|c| c.0 <= k as f64 + 0.5).unwrap_or(first).1 as f32).collect()
        } else if let Some(e) = overall {
            let mut sorted = self.loud.clone();
            sorted.sort_by(f32::total_cmp);
            let median = sorted.get(n / 2).copied().unwrap_or(0.0).max(0.02);
            self.loud.iter().map(|l| (e as f32 * (l / median).powf(0.7)).clamp(1.0, 10.0)).collect()
        } else {
            Vec::new()
        };
        self
    }

    /// With the song's sections (seconds; see `stems_core::structure`): each bar takes the label
    /// of the section it is mostly in, and every section starts on its nearest bar line.
    pub fn with_sections(mut self, sections: &[crate::stems::Section]) -> Self {
        let n = self.len();
        let bar = |t: f64| (t - self.start) / self.bar_secs;
        self.labels = (0..n)
            .map(|k| sections.iter().find(|s| bar(s.start) <= k as f64 + 0.5 && k as f64 + 0.5 < bar(s.end)).map_or("", |s| s.label))
            .collect();
        self.section_starts = sections.iter().map(|s| bar(s.start).round()).filter(|b| *b >= 1.0 && *b < n as f64).map(|b| b as usize).collect();
        self.section_starts.dedup();
        self
    }

    /// The section label of bar `k` ("" when unknown).
    pub fn label(&self, k: usize) -> &'static str {
        self.labels.get(k).copied().unwrap_or("")
    }

    /// A section starts at bar `k`.
    pub fn starts_section(&self, k: usize) -> bool {
        self.section_starts.contains(&k)
    }

    pub fn energy_at(&self, k: usize) -> Option<f32> {
        self.energy.get(k.min(self.energy.len().saturating_sub(1))).copied().filter(|_| !self.energy.is_empty())
    }

    /// Bar `k` starts a 4-bar phrase line / an 8-bar phrase.
    pub fn line4(&self, k: usize) -> bool {
        (k + 8 - self.phrase % 4).is_multiple_of(4)
    }

    pub fn line8(&self, k: usize) -> bool {
        (k + 8 - self.phrase).is_multiple_of(8)
    }

    /// Bar `k` is in a break: no bass, between bars with bass.
    pub fn in_break(&self, k: usize) -> bool {
        !self.on(BASS, k) && k >= self.intro_end() && k < self.outro_start()
    }
}

/// The bar (0–7) a track's 8-bar phrases start on, from its cue points (in bars of its grid):
/// Mixed In Key puts them on phrase starts, which needn't be the grid's first bar (a pickup, an
/// intro of 3 bars). 0 when most cues don't agree.
pub fn phrase_offset(cue_bars: &[f64]) -> usize {
    let on: Vec<usize> = cue_bars.iter().filter(|b| **b >= 0.5 && (*b - b.round()).abs() < 0.1).map(|b| (b.round() as usize) % 8).collect();
    let best = (0..8).max_by_key(|k| (on.iter().filter(|b| *b == k).count(), usize::MAX - k));
    match best {
        Some(k) if on.iter().filter(|b| **b == k).count() * 2 > on.len() && on.len() >= 2 => k,
        _ => 0,
    }
}

/// How a transition blends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Technique {
    /// The parts swap over the overlap (stems or EQ).
    Blend,
    /// No overlap: the new track cuts in as the old one ends.
    Cut,
}

/// One possible transition.
#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    /// Bar of the old track where it starts (the mix's first bar).
    pub out_bar: usize,
    /// Bar of the new track that plays at that moment.
    pub in_bar: usize,
    pub bars: usize,
    pub technique: Technique,
    pub score: f32,
    /// Why (printed when the mix starts).
    pub notes: Vec<String>,
}

/// What [`search`] may choose from.
#[derive(Clone, Debug)]
pub struct Limits {
    /// The old track's first bar a mix can start at (bars of its grid), and its last one.
    pub earliest: usize,
    pub latest: usize,
    /// Overlap lengths to try, in bars (every phrase length that could fit: the songs decide).
    pub lengths: Vec<usize>,
    /// The shortest blend, in bars (20 s at the tempo it plays at); shorter ones, and cuts, only
    /// from bar `short_from` of the old track on (where less than 30 s of it is left).
    pub min_bars: usize,
    pub short_from: usize,
    /// Best: the mix starts before this bar of the old track (while 30 s of it is left) and is
    /// over before it ends, unless nothing fits that way; `usize::MAX`: no such limit.
    pub start_before: usize,
    /// The bar of the new track at its cue (where it waits), if it's on a bar line.
    pub cue_bar: Option<usize>,
    /// You chose that cue: the new track comes in from it (every candidate starts it there).
    pub cue_only: bool,
    /// The bar of its drop cue (a hot cue named "Drop"), to cut in on.
    pub drop_cue_bar: Option<usize>,
    /// The bars of its other cue points (phrase starts): places it can come in.
    pub in_cues: Vec<usize>,
    pub blend: bool,
    pub cut: bool,
    /// Keys clash, the new vocals stay out (Now), stems on both decks.
    pub keys_clash: bool,
    pub vocals_out: bool,
    pub stems: bool,
    /// Prefer starting soon (Now) rather than waiting for the best moment (Best).
    pub hurry: bool,
}

/// Score one transition. Higher is better.
pub fn score(out: &Bars, inc: &Bars, c: &Candidate, lim: &Limits) -> (f32, Vec<String>) {
    let mut s = 0.0f32;
    let mut notes = Vec::new();
    let n_out = out.len();
    let ob = |k: usize| c.out_bar + k;
    // Bars of the mix the old track still plays (it may end, and fade on its own, before the mix
    // does: no rush).
    let playing = c.bars.min(n_out.saturating_sub(c.out_bar));
    let end_in = c.in_bar + c.bars;
    // A drop of the new track right as the old one has gone: the strongest fit.
    let drops = inc.drops();
    if c.technique == Technique::Blend && drops.contains(&end_in) {
        s += 6.0;
        notes.push(format!("its drop lands as the old track leaves (bar {})", end_in + 1));
    }
    if c.technique == Technique::Cut {
        if Some(c.in_bar) == lim.drop_cue_bar {
            // Your drop cue, for a cut: as strong as your cue is for a blend.
            s += 12.0;
            notes.push("cuts in on its drop cue".into());
        } else if drops.contains(&c.in_bar) {
            s += 5.0;
            notes.push("cuts in on its drop".into());
        } else if c.in_bar == inc.intro_end() {
            s += 2.0;
        }
        // Cutting while the old track is full is abrupt unless it's at its phrase end.
        if out.on(BASS, ob(0)) && !out.line8(c.out_bar) {
            s -= 2.0;
        }
    }
    let (mut vox, mut mel) = (0, 0);
    let mut good = 0.0f32;
    let mut holes = 0usize;
    let (mut out_room, mut in_room) = (0usize, 0usize);
    let (mut peak_out, mut peak_in) = (0usize, 0usize);
    if c.technique == Technique::Blend {
        for k in 0..playing {
            let (o, i) = (ob(k), c.in_bar + k);
            if out.on(VOCALS, o) && inc.on(VOCALS, i) && !lim.vocals_out {
                vox += 1;
            }
            if lim.keys_clash && out.on(OTHER, o) && inc.on(OTHER, i) {
                mel += 1;
            }
            // Both full is fine (in sync, the EQ and stems shape it); only clashes sound wrong.
            let full = out.fullness(o) >= 3 && inc.fullness(i) >= 3;
            let clash_here = (out.on(VOCALS, o) && inc.on(VOCALS, i) && !lim.vocals_out) || (lim.keys_clash && out.on(OTHER, o) && inc.on(OTHER, i));
            // Length counts only where it sounds right: a bar where the two fit counts fully, two
            // full tracks in sync half, a clash not at all; less after 16 and 32 bars, so a mix is
            // long where the songs allow it, not everywhere.
            let value = if clash_here { 0.0 } else if full { 0.5 } else { 1.0 };
            let weight = if k < 16 { 0.5 } else if k < 32 { 0.3 } else { 0.15 };
            good += value * weight;
            // A hole: neither track has drums or bass here (an outro's tail on an intro's gap).
            if !(out.on(DRUMS, o) || inc.on(DRUMS, i)) && !(out.on(BASS, o) || inc.on(BASS, i)) {
                holes += 1;
            }
            // The old track in its outro or a break, the new one in its intro: they leave room.
            if o >= out.outro_start() || out.in_break(o) || matches!(out.label(o), "outro" | "break" | "end") {
                out_room += 1;
            }
            if i < inc.intro_end() || matches!(inc.label(i), "intro" | "start") {
                in_room += 1;
            }
            // The song's peak parts: the old chorus faded under the new track is missed, and the
            // new chorus heard first under the old one is wasted.
            if matches!(out.label(o), "chorus" | "solo") {
                peak_out += 1;
            }
            if matches!(inc.label(i), "chorus" | "solo") {
                peak_in += 1;
            }
        }
        // Stems keep vocals and melodies apart (each part moves on its own): a smaller cost than
        // with the EQ alone.
        let (wv, wm) = if lim.stems { (0.6, 0.4) } else { (2.0, 1.5) };
        s -= wv * vox as f32 + wm * mel as f32;
        s += good - 0.6 * holes as f32;
        // Bars where the old track leaves room (outro, break) or the new one is still sparse
        // (intro) fit best.
        s += 0.15 * (out_room + in_room) as f32;
        s -= 0.15 * peak_out as f32 + 0.1 * peak_in as f32;
        if peak_out > 0 {
            notes.push(format!("over {peak_out} bars of the old chorus"));
        }
        let (fo, fi) = (out_room as f32 / playing.max(1) as f32, in_room as f32 / playing.max(1) as f32);
        if vox == 0 && (0..c.bars).any(|k| inc.on(VOCALS, c.in_bar + k) || out.on(VOCALS, ob(k))) {
            notes.push("no vocals on top of each other".into());
        }
        if fo + fi >= 1.0 {
            notes.push("the old outro or break meets the new intro".into());
        }
        // A natural bass swap: the new bass comes in, or the old one stops, inside the overlap.
        let natural = (1..c.bars).any(|k| (!inc.on(BASS, c.in_bar + k - 1) && inc.on(BASS, c.in_bar + k)) || (out.on(BASS, ob(k - 1)) && !out.on(BASS, ob(k))));
        if natural {
            s += 2.0;
        }
    }
    // The songs' sections: leave as the old one's outro or break begins (or at least at a section
    // change), be gone as a section ends, and land the new chorus as the old track leaves.
    if out.starts_section(c.out_bar) {
        let what = out.label(c.out_bar);
        s += if matches!(what, "outro" | "break") { 1.5 } else { 0.75 };
        notes.push(format!("leaves as its {what} begins"));
    }
    if out.starts_section(c.out_bar + c.bars) || (c.out_bar + c.bars >= n_out && !out.labels.is_empty()) {
        s += 0.75;
    }
    if inc.starts_section(c.in_bar) {
        s += 0.5;
    }
    if c.technique == Technique::Blend && !drops.contains(&end_in) && inc.starts_section(end_in) && inc.label(end_in) == "chorus" {
        s += 3.0;
        notes.push(format!("its chorus lands as the old track leaves (bar {})", end_in + 1));
    }
    // Real DJ mixes last whole 8-bar phrases far more often than the lengths between (the
    // DJ-mix studies: 1.5–1.65× as many at 8, 16, 24 and 32 bars as just around them).
    if c.technique == Technique::Blend && c.bars.is_multiple_of(8) {
        s += 0.5;
    }
    // Phrases: start and end on the old track's phrase lines; the new track from a phrase line.
    if out.line8(c.out_bar) {
        s += 1.0;
    }
    if out.line8(c.out_bar + c.bars) {
        s += 0.5;
    }
    if inc.line8(c.in_bar) || Some(c.in_bar) == lim.cue_bar {
        s += 0.5;
    }
    // The new track from its start (intro) rather than skipping into it (unless you cued it there).
    if Some(c.in_bar) != lim.cue_bar {
        s -= 0.04 * c.in_bar as f32;
    }
    // Best: the old track plays most of itself before it starts leaving.
    if !lim.hurry {
        let early = (n_out * 55 / 100).saturating_sub(c.out_bar);
        s -= 0.2 * early as f32;
    }
    // Your cue: a starting point, a small preference (the music decides where it comes in).
    let cut_on_drop_cue = c.technique == Technique::Cut && lim.drop_cue_bar.is_some();
    if Some(c.in_bar) == lim.cue_bar && !cut_on_drop_cue {
        s += 2.5;
    } else if c.in_bar != 0 {
        notes.push(format!("starts it at bar {}", c.in_bar + 1));
    }
    // Energy flow: after the mix level with, or a step above, the energy before it; a drop or a
    // big jump costs. While they overlap, the new track much hotter than the old (a peak under a
    // quiet outro) costs too.
    if let (Some(eb), Some(ea)) = (out.energy_at(c.out_bar.saturating_sub(1)), inc.energy_at(end_in)) {
        let step = ea - eb;
        if (0.0..=1.0).contains(&step) {
            s += 1.0;
        }
        s -= 0.6 * (-step - 0.5).max(0.0) + 0.4 * (step - 1.5).max(0.0);
        if step.abs() >= 1.0 {
            notes.push(format!("energy {eb:.0} → {ea:.0}"));
        }
    }
    if c.technique == Technique::Blend {
        let hotter = (0..playing).filter(|&k| match (out.energy_at(ob(k)), inc.energy_at(c.in_bar + k)) {
            (Some(o), Some(i)) => i > o + 1.5,
            _ => false,
        });
        s -= 0.1 * hotter.count() as f32;
    }
    // Loudness: the new track after the mix shouldn't jump far from the old one before it.
    let before = out.loud.get(c.out_bar.saturating_sub(1)).copied().unwrap_or(0.0);
    let after = inc.loud.get(end_in.min(inc.len().saturating_sub(1))).copied().unwrap_or(0.0);
    s -= 1.5 * (before - after).abs();
    // Waiting.
    let wait = c.out_bar.saturating_sub(lim.earliest) as f32;
    s -= if lim.hurry { 2.5 } else { 0.03 } * wait;
    if c.out_bar + c.bars > n_out {
        notes.push("the old track ends on its own during the mix".into());
    }
    (s, notes)
}

/// The best transitions (best first), from the limits.
pub fn search(out: &Bars, inc: &Bars, lim: &Limits) -> Vec<Candidate> {
    let n_out = out.len();
    let n_in = inc.len();
    // Where the new track can come in: your cue, its start, the phrase lines before its drops
    // (a build), its drops (to cut in on), its intro end.
    let mut entries: Vec<usize> = Vec::new();
    entries.extend(lim.cue_bar);
    let cue_only = lim.cue_only && lim.cue_bar.is_some();
    entries.extend(lim.drop_cue_bar);
    entries.extend(lim.in_cues.iter().copied());
    entries.push(0);
    entries.extend(inc.section_starts.iter().copied());
    let choruses = inc.section_starts.iter().copied().filter(|&k| inc.label(k) == "chorus");
    for d in inc.drops().iter().copied().chain([inc.intro_end()]).chain(choruses) {
        for back in [0, 4, 8, 16] {
            if d >= back {
                entries.push(d - back);
            }
        }
    }
    if cue_only {
        entries.retain(|b| Some(*b) == lim.cue_bar);
    }
    entries.sort_unstable();
    entries.dedup();
    // Most of the new track plays on after the mix (half of it, at least 32 bars); with a track
    // too short for that, at least 8 bars.
    let rest = |strict: bool| if strict { (n_in / 2).max(32) } else { 8 };
    let mut found = Vec::new();
    // In time first (see `start_before`); the very end only when nothing else fits.
    let timing = if lim.start_before == usize::MAX { vec![false] } else { vec![true, false] };
    for (timed, strict) in timing.iter().flat_map(|&t| [(t, true), (t, false)]) {
        for out_bar in lim.earliest..=lim.latest.min(n_out.saturating_sub(1)) {
            // On the old track's phrase lines (every 4 bars, from its cues' phrase start).
            if !out.line4(out_bar) {
                continue;
            }
            for &in_bar in &entries {
                for (tech, lengths) in [(Technique::Blend, lim.lengths.clone()), (Technique::Cut, vec![1])] {
                    if (tech == Technique::Blend && !lim.blend) || (tech == Technique::Cut && !lim.cut) {
                        continue;
                    }
                    for &bars in &lengths {
                        // Never a quick mix (a short blend, a cut) unless the old track is almost
                        // over.
                        if out_bar < lim.short_from && (tech == Technique::Cut || bars < lim.min_bars) {
                            continue;
                        }
                        if timed && (out_bar >= lim.start_before || out_bar + bars > n_out) {
                            continue;
                        }
                        if in_bar + bars + rest(strict) > n_in {
                            continue;
                        }
                        // The old track may end (and fade on its own) during the mix, up to a
                        // phrase before the mix does; it plays at least 2 bars of it.
                        if out_bar + bars > n_out + 4 || out_bar + 2 > n_out {
                            continue;
                        }
                        let mut c = Candidate { out_bar, in_bar, bars, technique: tech, score: 0.0, notes: Vec::new() };
                        let (s, notes) = score(out, inc, &c, lim);
                        c.score = s;
                        c.notes = notes;
                        found.push(c);
                    }
                }
            }
        }
        if !found.is_empty() {
            break;
        }
    }
    found.sort_by(|a, b| b.score.total_cmp(&a.score));
    // Distinct choices: alternatives that differ in where the new track comes in, how long or
    // how, not the same transition a few bars later.
    let mut best: Vec<Candidate> = Vec::new();
    for c in found {
        if best.iter().all(|b| (b.in_bar, b.bars, b.technique) != (c.in_bar, c.bars, c.technique)) {
            best.push(c);
        }
        if best.len() == 3 {
            break;
        }
    }
    best
}

/// A mashup: where the new track's singing (`inc`) rides over the old track's music (`out`, its
/// own vocals out) before the new track's music comes in under it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Mashup {
    /// The new track's bar it starts on (a phrase start where it sings).
    pub in_bar: usize,
    /// The old track's bar it starts on (a phrase start from `earliest`).
    pub out_bar: usize,
    /// Bars of the new vocals over the old music (8–32, whole 8-bar phrases).
    pub bars: usize,
}

/// The longest stretch of singing to bring (8 to 32 bars from a phrase start of `inc`; a chorus
/// first), over the old track's music from its first phrase start from `earliest` (`now`), or
/// where its music is fullest and its own singing least (otherwise), with 2 bars after for
/// the new music to take over. `None` when the new track has no 8 bars of singing, or the old
/// track has no room.
pub fn find_mashup(inc: &Bars, out: &Bars, earliest: usize, now: bool) -> Option<Mashup> {
    const TAIL: usize = 2;
    let on_phrase = |b: &Bars, k: usize| (k + 8 - b.phrase % 8) % 8 == 0;
    // Bars of singing from bar k: 4-bar blocks with vocals in at least half their bars.
    let sung = |k: usize| {
        let mut n = 0;
        while n < 32 && k + n + 4 <= inc.len() && (k + n..k + n + 4).filter(|&j| inc.on(VOCALS, j)).count() >= 2 {
            n += 4;
        }
        n / 8 * 8
    };
    let (in_bar, bars) = (0..inc.len())
        .filter(|&k| on_phrase(inc, k) && inc.on(VOCALS, k))
        .map(|k| (k, sung(k)))
        .filter(|&(_, n)| n >= 8)
        .max_by_key(|&(k, n)| (n.min(16), inc.label(k) == "chorus", n, std::cmp::Reverse(k)))?;
    // The old track: its phrase starts from `earliest` with room for it (shorter if that's all).
    let room = |k: usize| out.len().saturating_sub(k + TAIL) / 8 * 8;
    let starts = (earliest..out.len()).filter(|&k| on_phrase(out, k) && room(k) >= 8);
    let music = |k: usize, n: usize| (k..k + n).map(|j| [DRUMS, BASS, OTHER].iter().filter(|&&p| out.on(p, j)).count()).sum::<usize>() as f64 / n as f64;
    let sings = |k: usize, n: usize| (k..k + n).filter(|&j| out.on(VOCALS, j)).count() as f64 / n as f64;
    let out_bar = if now {
        starts.into_iter().next()?
    } else {
        // (A little sooner over later.)
        let score = |k: usize| {
            let n = bars.min(room(k));
            music(k, n) - sings(k, n) + n as f64 / 32.0 - 0.3 * (k - earliest) as f64 / out.len().max(1) as f64
        };
        starts.max_by(|&a, &b| score(a).total_cmp(&score(b)))?
    };
    Some(Mashup { in_bar, out_bar, bars: bars.min(room(out_bar)) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vocal_share_counts_the_bars_with_music() {
        // 10 bars: silent for 2, then drums; vocals in bars 2-4.
        let lv = |r: (usize, usize)| (0..10).map(|k| if (r.0..r.1).contains(&k) { 0.5 } else { 0.0 }).collect::<Vec<f32>>();
        let bars = Bars { start: 0.0, bar_secs: 2.0, stems: Some([lv((2, 10)), lv((0, 0)), lv((2, 5)), lv((0, 0))]), bands: [lv((2, 10)), lv((2, 10)), lv((2, 10))], loud: lv((2, 10)), ..Default::default() };
        assert_eq!(bars.vocal_share(), Some(38), "3 of the 8 bars with music");
        assert_eq!(Bars { stems: None, ..bars }.vocal_share(), None, "no stems: not known");
    }

    /// A track of `n` bars: (from, to) bar ranges where each stem plays; loudness follows.
    pub fn track(n: usize, ranges: [&[(usize, usize)]; 4]) -> Bars {
        let stems: [Vec<f32>; 4] =
            std::array::from_fn(|s| (0..n).map(|k| if ranges[s].iter().any(|&(a, b)| (a..b).contains(&k)) { 0.5 } else { 0.0 }).collect());
        let loud = (0..n).map(|k| 0.2 + 0.2 * (0..4).filter(|&s| stems[s][k] > 0.0).count() as f32).collect();
        Bars { start: 0.0, bar_secs: 2.0, bands: [vec![0.3; n], vec![0.3; n], vec![0.3; n]], loud, stems: Some(stems), ..Default::default() }.finished()
    }

    #[test]
    fn a_mashup_brings_the_longest_singing_over_the_fullest_music() {
        // New: an 8-bar intro, singing 8-32 (24 bars), a break, singing again 40-48.
        let inc = track(56, [&[(0, 56)], &[(8, 56)], &[(8, 32), (40, 48)], &[(0, 56)]]);
        // Old: full music but a thin breakdown 32-40 (only other), singing 48-56.
        let out = track(72, [&[(0, 32), (40, 72)], &[(0, 32), (40, 72)], &[(48, 56)], &[(0, 72)]]);
        let m = find_mashup(&inc, &out, 2, false).expect("a mashup");
        assert_eq!((m.in_bar, m.bars), (8, 24), "the long singing, in whole phrases: {m:?}");
        assert!(m.out_bar % 8 == 0 && m.out_bar >= 2, "{m:?}");
        // Over full music where the old track doesn't sing (its vocals would be out anyway).
        assert_eq!(m.out_bar, 8, "{m:?}");
        // Now: the next phrase start, whatever plays there.
        assert_eq!(find_mashup(&inc, &out, 10, true).unwrap().out_bar, 16);
        // Near the end of the old track: shorter, to fit.
        assert_eq!(find_mashup(&inc, &out, 50, true).map(|m| (m.out_bar, m.bars)), Some((56, 8)));
        assert_eq!(find_mashup(&inc, &out, 60, true), None, "no room left");
        // No singing in the new track: no mashup.
        let inst = track(56, [&[(0, 56)], &[(8, 56)], &[], &[(0, 56)]]);
        assert_eq!(find_mashup(&inst, &out, 2, false), None);
    }

    fn limits(earliest: usize, latest: usize) -> Limits {
        Limits {
            earliest,
            latest,
            lengths: (2..=16).map(|k| 4 * k).collect(),
            min_bars: 0,
            short_from: 0,
            start_before: usize::MAX,
            cue_bar: None,
            cue_only: false,
            drop_cue_bar: None,
            in_cues: vec![],
            blend: true,
            cut: true,
            keys_clash: false,
            vocals_out: false,
            stems: true,
            hurry: false,
        }
    }

    /// Sections for a track of 2 s bars: (first bar, label).
    fn sections(parts: &[(usize, &'static str)], n: usize) -> Vec<crate::stems::Section> {
        parts
            .iter()
            .enumerate()
            .map(|(k, &(b, label))| crate::stems::Section { start: 2.0 * b as f64, end: 2.0 * parts.get(k + 1).map_or(n, |p| p.0) as f64, label })
            .collect()
    }

    #[test]
    fn the_songs_sections_say_where_to_leave_and_land() {
        // Two songs that play everything throughout (the stems alone can't tell their parts
        // apart), vocals in their verses and choruses.
        let out_parts = [(0, "intro"), (16, "verse"), (32, "chorus"), (48, "verse"), (64, "chorus"), (80, "outro")];
        let in_parts = [(0, "intro"), (16, "verse"), (32, "chorus"), (48, "verse"), (64, "chorus"), (96, "outro")];
        let out = track(96, [&[(0, 96)], &[(0, 96)], &[(16, 80)], &[(0, 96)]]).with_sections(&sections(&out_parts, 96));
        let inc = track(128, [&[(0, 128)], &[(0, 128)], &[(16, 96)], &[(0, 128)]]).with_sections(&sections(&in_parts, 128));
        assert_eq!((out.label(40), out.label(80), inc.label(0)), ("chorus", "outro", "intro"));
        assert!(out.starts_section(80) && !out.starts_section(81));
        let best = search(&out, &inc, &limits(40, 90));
        let b = &best[0];
        // It starts leaving as a section of the old song begins (its last chorus: the new intro
        // and verse ride under its chorus and outro, no vocals on top of each other) and the new
        // chorus lands as the old song is gone.
        assert!(out.starts_section(b.out_bar), "{b:?}");
        assert!(b.notes.iter().any(|n| n.contains("chorus lands")), "{b:?}");
        assert_eq!(inc.label(b.in_bar + b.bars), "chorus", "{b:?}");
        assert!(b.notes.iter().any(|n| n.contains("no vocals on top")), "{b:?}");
        // Without the sections it can't know where the songs' parts are.
        let plain = search(&track(96, [&[(0, 96)], &[(0, 96)], &[(16, 80)], &[(0, 96)]]), &track(128, [&[(0, 128)], &[(0, 128)], &[(16, 96)], &[(0, 128)]]), &limits(40, 90));
        assert!(plain[0].notes.iter().all(|n| !n.contains("chorus")), "{:?}", plain[0]);
    }

    #[test]
    fn mixes_last_whole_phrases() {
        // Two tracks that fit anywhere: of the lengths that fit equally, a whole number of 8-bar
        // phrases (as real DJ mixes mostly are).
        let t = || track(160, [&[(0, 160)], &[(0, 160)], &[], &[(0, 160)]]);
        let best = search(&t(), &t(), &limits(40, 120));
        assert!(best[0].technique == Technique::Blend && best[0].bars.is_multiple_of(8), "{best:?}");
    }

    #[test]
    fn sections_come_from_the_bass() {
        // Intro 0-8 without bass, bass 8-40, break 40-48, drop at 48, outro from 80.
        let t = track(96, [&[(0, 96)], &[(8, 40), (48, 80)], &[(16, 40)], &[(0, 96)]]);
        assert_eq!(t.intro_end(), 8);
        assert_eq!(t.drops(), &[8, 48]);
        assert_eq!(t.outro_start(), 80);
        assert!(t.in_break(44) && !t.in_break(20) && !t.in_break(4));
    }

    #[test]
    fn the_old_outro_rides_under_the_new_intro() {
        // Old: full until bar 64, then an outro (drums and other) to 96.
        let out = track(96, [&[(0, 96)], &[(0, 64)], &[(16, 56)], &[(0, 96)]]);
        // New: a 16-bar intro (drums and other), its drop at bar 16, vocals from 24.
        let inc = track(96, [&[(0, 96)], &[(16, 96)], &[(24, 60)], &[(0, 96)]]);
        let best = search(&out, &inc, &limits(40, 80));
        let b = &best[0];
        assert_eq!(b.technique, Technique::Blend);
        // As long as the two fit: the old outro (no bass, no vocals) under the new intro.
        assert!(b.bars >= 16, "{b:?}");
        assert!(b.out_bar + b.bars > 64, "reaches the old outro: {b:?}");
        assert!(best.len() > 1, "alternatives");
        for (k, a) in best.iter().enumerate() {
            for b in &best[k + 1..] {
                assert_ne!((a.in_bar, a.bars, a.technique), (b.in_bar, b.bars, b.technique), "alternatives differ");
            }
        }
    }

    #[test]
    fn a_cue_you_chose_is_where_the_new_track_comes_in() {
        // The new track's drop at bar 32 (bass from there) draws a mix to it; your cue is bar 8.
        let out = track(96, [&[(0, 96)], &[(0, 80)], &[], &[(0, 96)]]);
        let inc = track(128, [&[(0, 128)], &[(32, 128)], &[], &[(0, 128)]]);
        let mut lim = limits(40, 80);
        lim.cue_bar = Some(8);
        let free = search(&out, &inc, &lim);
        assert!(free.iter().any(|c| c.in_bar != 8), "otherwise it may start elsewhere");
        lim.cue_only = true;
        let found = search(&out, &inc, &lim);
        assert!(!found.is_empty());
        assert!(found.iter().all(|c| c.in_bar == 8), "{found:?}");
    }

    #[test]
    fn in_a_hurry_it_starts_soon_and_your_cue_is_preferred() {
        let out = track(96, [&[(0, 96)]; 4]);
        let inc = track(96, [&[(0, 96)]; 4]);
        let mut lim = limits(20, 28);
        lim.hurry = true;
        lim.cue_bar = Some(32);
        lim.cut = false;
        lim.lengths = vec![4];
        let b = &search(&out, &inc, &lim)[0];
        assert_eq!(b.out_bar, 20);
        assert_eq!(b.in_bar, 32, "your cue");
    }

    #[test]
    fn no_quick_mix_until_the_old_track_is_almost_over() {
        // 2 s bars: 20 s is 10 bars; the old track's last 30 s are its last 15 bars (from 81).
        let out = track(96, [&[(0, 96)]; 4]);
        let inc = track(96, [&[(0, 96)]; 4]);
        let mut lim = limits(0, 95);
        lim.lengths = vec![4, 8, 12, 16];
        lim.min_bars = 10;
        lim.short_from = 81;
        let found = search(&out, &inc, &lim);
        assert!(!found.is_empty());
        for c in &found {
            let quick = c.technique == Technique::Cut || c.bars < 10;
            assert!(!quick || c.out_bar >= 81, "{c:?}");
        }
        // Only cuts allowed (keys or tempos that don't blend): they wait for the end.
        lim.blend = false;
        let found = search(&out, &inc, &lim);
        assert!(!found.is_empty() && found.iter().all(|c| c.out_bar >= 81), "{found:?}");
        // In a hurry, well before the end: nothing (dj blends 20 s instead).
        lim.earliest = 20;
        lim.latest = 24;
        assert!(search(&out, &inc, &lim).is_empty());
    }

    #[test]
    fn best_starts_before_the_last_30_seconds_and_is_over_before_the_end() {
        // The old track sings to bar 88, then an 8-bar outro without drums or bass to the end:
        // that outro (its last 16 s) draws the mix there.
        let out_parts = [(0, "intro"), (16, "verse"), (32, "chorus"), (48, "verse"), (64, "chorus"), (88, "outro")];
        let out = track(96, [&[(0, 88)], &[(0, 88)], &[(16, 88)], &[(0, 96)]]).with_sections(&sections(&out_parts, 96));
        let inc = track(128, [&[(0, 128)], &[(8, 128)], &[(24, 96)], &[(0, 128)]]);
        let mut lim = limits(40, 95);
        lim.cut = false;
        let free = &search(&out, &inc, &lim)[0];
        assert!(free.out_bar >= 81, "without the limit it waits for the very end: {free:?}");
        // 2 s bars: the last 30 s are the last 15 bars.
        lim.start_before = 81;
        let best = search(&out, &inc, &lim);
        for b in &best {
            assert!(b.out_bar < 81 && b.out_bar + b.bars <= 96, "{b:?}");
        }
        // Asked when it's already that late: still a mix (the end is all there is).
        lim.earliest = 84;
        assert!(!search(&out, &inc, &lim).is_empty());
    }

    #[test]
    fn the_old_track_may_end_during_the_mix() {
        let out = track(40, [&[(0, 40)], &[(0, 40)], &[], &[(0, 40)]]);
        let inc = track(96, [&[(0, 96)], &[(16, 96)], &[], &[(0, 96)]]);
        let mut lim = limits(36, 36);
        lim.lengths = vec![8];
        lim.cut = false;
        let b = &search(&out, &inc, &lim)[0];
        assert_eq!((b.out_bar, b.bars), (36, 8), "{b:?}");
        assert!(b.notes.iter().any(|n| n.contains("ends on its own")), "{:?}", b.notes);
        // But not far past its end.
        lim.lengths = vec![16];
        assert!(search(&out, &inc, &lim).is_empty());
    }

    #[test]
    fn how_long_comes_from_the_songs() {
        // Two full tracks with vocals all the way, EQ only: vocals on vocals, so the shortest.
        let full = track(96, [&[(0, 96)]; 4]);
        let mut lim = limits(40, 48);
        lim.cut = false;
        lim.stems = false;
        let b = &search(&full, &full, &lim)[0];
        assert_eq!(b.bars, 8, "{b:?}");
        // The same with stems and the new vocals out (Now): the stems take them apart, so long.
        lim.stems = true;
        lim.vocals_out = true;
        let b = &search(&full, &full, &lim)[0];
        assert!(b.bars >= 16, "{b:?}");
        lim.vocals_out = false;
        // A long sparse outro and a long sparse intro: a long mix.
        let out = track(96, [&[(0, 96)], &[(0, 40)], &[(0, 40)], &[(0, 96)]]);
        let inc = track(96, [&[(0, 96)], &[(32, 96)], &[(40, 80)], &[(0, 96)]]);
        let b = &search(&out, &inc, &lim)[0];
        assert!(b.bars >= 32, "{b:?}");
    }

    #[test]
    fn phrases_come_from_the_cues() {
        // Too Good's cues (bars of its grid): phrases start 3 bars in.
        assert_eq!(phrase_offset(&[-0.95, 7.0, 35.0, 43.0, 75.0, 91.0, 99.0, 107.0]), 3);
        // Push's: on the grid's bars.
        assert_eq!(phrase_offset(&[0.0, 16.0, 24.0, 84.0, 92.0, 136.0, 144.0]), 0);
        assert_eq!(phrase_offset(&[]), 0);
        let t = track(32, [&[(0, 32)]; 4]).with_phrase(3);
        assert!(t.line4(7) && t.line4(3) && !t.line4(4) && t.line8(11) && !t.line8(7));
    }

    #[test]
    fn the_new_track_keeps_most_of_itself_and_best_waits_for_the_old_ones_end() {
        // Two sparse tracks that fit anywhere: no reason to mix early or to use up the new one.
        let out = track(128, [&[(0, 128)], &[(0, 100)], &[], &[(0, 128)]]);
        let inc = track(128, [&[(0, 128)], &[(16, 128)], &[], &[(0, 128)]]);
        let mut lim = limits(8, 127);
        lim.cut = false;
        let b = &search(&out, &inc, &lim)[0];
        assert!(b.in_bar + b.bars + 64 <= 128, "half of the new track left: {b:?}");
        assert!(b.out_bar >= 64, "the old track plays most of itself first: {b:?}");
    }

    #[test]
    fn gaps_in_both_tracks_are_not_lined_up() {
        // The old track's drums and bass stop at bar 60; the new intro has a 4-bar gap at 8-12.
        let out = track(96, [&[(0, 60)], &[(0, 60)], &[], &[(0, 96)]]);
        let inc = track(96, [&[(0, 8), (12, 96)], &[(12, 96)], &[], &[(0, 96)]]);
        let lim = limits(40, 60);
        let at = |out_bar| score(&out, &inc, &Candidate { out_bar, in_bar: 0, bars: 16, technique: Technique::Blend, score: 0.0, notes: vec![] }, &lim).0;
        // From bar 52 the gap lands on the old track's silence (4 bars with neither drums nor
        // bass); from 48 the old drums fill it.
        assert!(at(48) > at(52), "{} {}", at(48), at(52));
    }

    #[test]
    fn energy_flows_up_not_down() {
        // The old track ends at energy 6. The new one: an energy-4 break at bars 32-48, energy 7
        // from bar 48 (cues at 0: 6, 32: 4, 48: 7).
        let out = track(96, [&[(0, 96)], &[(0, 96)], &[], &[(0, 96)]]).with_energy(&[(0.0, 6)], None);
        let inc = track(128, [&[(0, 128)], &[(0, 128)], &[], &[(0, 128)]]).with_energy(&[(0.0, 6), (32.0, 4), (48.0, 7)], None);
        assert_eq!(inc.energy_at(40), Some(4.0));
        let lim = limits(64, 64);
        let at = |in_bar, bars| score(&out, &inc, &Candidate { out_bar: 64, in_bar, bars, technique: Technique::Blend, score: 0.0, notes: vec![] }, &lim);
        // Ending the mix into the energy-4 break (a drop of 2) scores below ending into the energy
        // 6 or 7 sections.
        let (drop, _) = at(24, 16);
        let (up, notes) = at(32, 16);
        assert!(up > drop, "{up} {drop}");
        assert!(notes.iter().any(|n| n.contains("energy 6 → 7")), "{notes:?}");
        // Without cues: the overall energy shaped by loudness.
        let plain = track(32, [&[(0, 32)]; 4]).with_energy(&[], Some(6));
        assert_eq!(plain.energy_at(10), Some(6.0));
    }
}
