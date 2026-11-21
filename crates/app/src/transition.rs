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

/// A track, bar by bar. Bar 0 starts at the grid's beat 0.
#[derive(Clone, Debug, Default)]
pub struct Bars {
    /// Track time of bar 0 and the length of a bar, in seconds.
    pub start: f64,
    pub bar_secs: f64,
    /// Per stem (when separated): mean level per bar.
    pub stems: Option<[Vec<f32>; 4]>,
    /// Low, mid, high band and peak loudness per bar (from the waveform).
    pub bands: [Vec<f32>; 3],
    pub loud: Vec<f32>,
    /// Sections, worked out once (see [`Bars::finished`]).
    drop_bars: Vec<usize>,
    intro: usize,
    outro: usize,
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

    /// Bar `k` is in a break: no bass, between bars with bass.
    pub fn in_break(&self, k: usize) -> bool {
        !self.on(BASS, k) && k >= self.intro_end() && k < self.outro_start()
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
    /// The old track loops its last phrase from this bar (it would end before the mix does).
    pub loop_from: Option<usize>,
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
    /// The bar of the new track at its cue (where it waits), if it's on a bar line.
    pub cue_bar: Option<usize>,
    /// The bar of its drop cue (a hot cue named "Drop"), to cut in on.
    pub drop_cue_bar: Option<usize>,
    pub blend: bool,
    pub cut: bool,
    /// Keys clash, the new vocals stay out (Now), stems on both decks.
    pub keys_clash: bool,
    pub vocals_out: bool,
    pub stems: bool,
    /// Prefer starting soon (Now) rather than waiting for the best moment (Best).
    pub hurry: bool,
}

/// The old track's bar playing `k` bars into a transition starting at `start`, with its last
/// phrase looping from `loop_from` (if it would run out).
pub fn out_bar(start: usize, k: usize, loop_from: Option<usize>, n: usize) -> usize {
    let b = start + k;
    match loop_from {
        Some(l) if b >= n && n > l => l + (b - l) % (n - l),
        _ => b,
    }
}

/// Score one transition. Higher is better.
pub fn score(out: &Bars, inc: &Bars, c: &Candidate, lim: &Limits) -> (f32, Vec<String>) {
    let mut s = 0.0f32;
    let mut notes = Vec::new();
    let n_out = out.len();
    let ob = |k: usize| out_bar(c.out_bar, k, c.loop_from, n_out);
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
        if out.on(BASS, ob(0)) && !c.out_bar.is_multiple_of(8) {
            s -= 2.0;
        }
    }
    let (mut vox, mut mel, mut busy, mut fit) = (0, 0, 0, 0);
    let (mut out_room, mut in_room) = (0usize, 0usize);
    if c.technique == Technique::Blend {
        for k in 0..c.bars {
            let (o, i) = (ob(k), c.in_bar + k);
            if out.on(VOCALS, o) && inc.on(VOCALS, i) && !lim.vocals_out {
                vox += 1;
            }
            if lim.keys_clash && out.on(OTHER, o) && inc.on(OTHER, i) {
                mel += 1;
            }
            let full = out.fullness(o) >= 3 && inc.fullness(i) >= 3;
            if full {
                busy += 1;
            }
            let clash_here = (out.on(VOCALS, o) && inc.on(VOCALS, i) && !lim.vocals_out) || (lim.keys_clash && out.on(OTHER, o) && inc.on(OTHER, i));
            if !full && !clash_here {
                fit += 1;
            }
            // The old track in its outro or a break, the new one in its intro: they leave room.
            if o >= out.outro_start() || out.in_break(o) {
                out_room += 1;
            }
            if i < inc.intro_end() {
                in_room += 1;
            }
        }
        // Stems can keep vocals and melodies apart (switching at the swap): a smaller cost.
        let (wv, wm) = if lim.stems { (0.6, 0.4) } else { (2.0, 1.5) };
        s -= wv * vox as f32 + wm * mel as f32 + 0.5 * busy as f32;
        // Every bar the two play well together counts: the longer the songs allow, the better
        // (bars that clash or crowd count against it, so it's as long as the music lets it be).
        s += 0.5 * fit as f32;
        // Bars where the old track leaves room (outro, break) or the new one is still sparse
        // (intro) fit best.
        s += 0.15 * (out_room + in_room) as f32;
        let (fo, fi) = (out_room as f32 / c.bars as f32, in_room as f32 / c.bars as f32);
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
    // Phrases: start and end on the old track's phrase lines; the new track from a phrase line.
    if c.out_bar.is_multiple_of(8) {
        s += 1.0;
    }
    if (c.out_bar + c.bars).is_multiple_of(8) {
        s += 0.5;
    }
    if c.in_bar.is_multiple_of(8) || Some(c.in_bar) == lim.cue_bar {
        s += 0.5;
    }
    // Your cue: where you chose to start it. It wins unless a mix from there can't work (other
    // entries only when the cue is too late in the track, or for a cut on a drop).
    let cut_on_drop_cue = c.technique == Technique::Cut && lim.drop_cue_bar.is_some();
    if Some(c.in_bar) == lim.cue_bar && !cut_on_drop_cue {
        s += 12.0;
    } else if c.in_bar != 0 {
        notes.push(format!("starts it at bar {}", c.in_bar + 1));
    }
    // Loudness: the new track after the mix shouldn't jump far from the old one before it.
    let before = out.loud.get(c.out_bar.saturating_sub(1)).copied().unwrap_or(0.0);
    let after = inc.loud.get(end_in.min(inc.len().saturating_sub(1))).copied().unwrap_or(0.0);
    s -= 1.5 * (before - after).abs();
    // Waiting, and looping the old track to wait.
    let wait = c.out_bar.saturating_sub(lim.earliest) as f32;
    s -= if lim.hurry { 2.5 } else { 0.03 } * wait;
    if let Some(l) = c.loop_from {
        let looped = (c.out_bar + c.bars).saturating_sub(n_out.max(l));
        s -= 0.25 * looped as f32;
        notes.push("loops the old outro to wait".into());
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
    entries.extend(lim.drop_cue_bar);
    entries.push(0);
    for d in inc.drops().iter().copied().chain([inc.intro_end()]) {
        for back in [0, 4, 8, 16] {
            if d >= back {
                entries.push(d - back);
            }
        }
    }
    entries.sort_unstable();
    entries.dedup();
    // The old track's last phrase (8 bars) to loop if it would end mid-mix.
    let last_phrase = (n_out / 8).saturating_sub(1) * 8;
    let mut found = Vec::new();
    for out_bar in lim.earliest..=lim.latest.min(n_out.saturating_sub(1)) {
        // Phrase lines only (4 bars).
        if !out_bar.is_multiple_of(4) {
            continue;
        }
        for &in_bar in &entries {
            for (tech, lengths) in [(Technique::Blend, lim.lengths.clone()), (Technique::Cut, vec![1])] {
                if (tech == Technique::Blend && !lim.blend) || (tech == Technique::Cut && !lim.cut) {
                    continue;
                }
                for &bars in &lengths {
                    if in_bar + bars + 8 > n_in {
                        continue; // the new track must play on after the mix
                    }
                    // It would end mid-mix: it loops from the mix's start to its end (its last
                    // phrase or less).
                    let loop_from = (out_bar + bars > n_out).then_some(out_bar);
                    if loop_from.is_some() && (out_bar < last_phrase || n_out < 8 || n_out - out_bar < 2) {
                        continue;
                    }
                    let mut c = Candidate { out_bar, in_bar, bars, technique: tech, loop_from, score: 0.0, notes: Vec::new() };
                    let (s, notes) = score(out, inc, &c, lim);
                    c.score = s;
                    c.notes = notes;
                    found.push(c);
                }
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A track of `n` bars: (from, to) bar ranges where each stem plays; loudness follows.
    pub fn track(n: usize, ranges: [&[(usize, usize)]; 4]) -> Bars {
        let stems: [Vec<f32>; 4] =
            std::array::from_fn(|s| (0..n).map(|k| if ranges[s].iter().any(|&(a, b)| (a..b).contains(&k)) { 0.5 } else { 0.0 }).collect());
        let loud = (0..n).map(|k| 0.2 + 0.2 * (0..4).filter(|&s| stems[s][k] > 0.0).count() as f32).collect();
        Bars { start: 0.0, bar_secs: 2.0, bands: [vec![0.3; n], vec![0.3; n], vec![0.3; n]], loud, stems: Some(stems), ..Default::default() }.finished()
    }

    fn limits(earliest: usize, latest: usize) -> Limits {
        Limits {
            earliest,
            latest,
            lengths: (1..=16).map(|k| 4 * k).collect(),
            cue_bar: None,
            drop_cue_bar: None,
            blend: true,
            cut: true,
            keys_clash: false,
            vocals_out: false,
            stems: true,
            hurry: false,
        }
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
        assert!(b.notes.iter().any(|n| n.contains("outro")), "{:?}", b.notes);
        assert!(best.len() > 1, "alternatives");
        for (k, a) in best.iter().enumerate() {
            for b in &best[k + 1..] {
                assert_ne!((a.in_bar, a.bars, a.technique), (b.in_bar, b.bars, b.technique), "alternatives differ");
            }
        }
    }

    #[test]
    fn in_a_hurry_it_starts_soon_and_your_cue_is_preferred() {
        let out = track(96, [&[(0, 96)]; 4]);
        let inc = track(96, [&[(0, 96)]; 4]);
        let mut lim = limits(20, 28);
        lim.hurry = true;
        lim.cue_bar = Some(32);
        lim.lengths = vec![4];
        let b = &search(&out, &inc, &lim)[0];
        assert_eq!(b.out_bar, 20);
        assert_eq!(b.in_bar, 32, "your cue");
    }

    #[test]
    fn the_old_track_loops_when_it_would_end_mid_mix() {
        let out = track(40, [&[(0, 40)], &[(0, 40)], &[], &[(0, 40)]]);
        let inc = track(96, [&[(0, 96)], &[(16, 96)], &[], &[(0, 96)]]);
        let mut lim = limits(32, 39);
        lim.lengths = vec![16];
        lim.cut = false;
        let b = &search(&out, &inc, &lim)[0];
        assert_eq!(b.loop_from, Some(32), "{b:?}");
        assert!(b.notes.iter().any(|n| n.contains("loops")));
    }

    #[test]
    fn how_long_comes_from_the_songs() {
        // Two full tracks with vocals all the way: they don't fit together, so a short mix.
        let full = track(96, [&[(0, 96)]; 4]);
        let mut lim = limits(40, 48);
        lim.cut = false;
        let b = &search(&full, &full, &lim)[0];
        assert_eq!(b.bars, 4, "{b:?}");
        // A long sparse outro and a long sparse intro: a long mix.
        let out = track(96, [&[(0, 96)], &[(0, 40)], &[(0, 40)], &[(0, 96)]]);
        let inc = track(96, [&[(0, 96)], &[(32, 96)], &[(40, 80)], &[(0, 96)]]);
        let b = &search(&out, &inc, &lim)[0];
        assert!(b.bars >= 32, "{b:?}");
    }
}
