# dj

Live-performance DJ software written entirely in Rust. It's modelled on the
feature set of Untitled Audio Syndicate's *Syndicate*: four decks, a full mixer
with **VST3 channel inserts** (User Guide §7.4), sampler decks, a shared master
clock, and MIDI/OSC control. It has its own name and code. (It was called Collective; the
first start moves the old `collective` settings folder to `dj`, and the old `/Collective/` OSC
prefix becomes `/dj/`.)

It's built for a **Mixed In Key** workflow: it reads the BPM, key, energy and cue points Mixed In
Key wrote into your files. Tracks without them get BPM, beat grid, energy and cue points from
dj's own analysis; keys are never guessed.

## Build & run

Requires Rust 1.95+ (stable).

```bash
cargo build --release
```

```bash
./target/release/dj              # GUI
./target/release/dj --headless   # engine + OSC/MIDI only (no window)
./target/release/dj --info track.mp3   # show the tags dj reads
./target/release/dj --list-devices
```

macOS app bundle and disk image, Apple Silicon only (see [packaging/macos](packaging/macos/README.md)):

```bash
cargo xtask package-macos
```

Windows: `cargo build --release` gives `target/release/dj.exe` (Visual Studio Build Tools needed).
CI builds it on every push (`dj-windows` artifact: a zip with `dj.exe`).

Linux needs the ALSA development headers to build (`libasound2-dev` on
Debian/Ubuntu). The GUI uses wgpu and falls back to OpenGL automatically; set
`DJ_RENDERER=glow` to force OpenGL.

Settings, session, mappings, FX presets and per-track edits are stored as
readable JSON in your config directory (`~/.config/dj` on Linux,
`~/Library/Application Support/dj` on macOS,
`%APPDATA%\dj` on Windows). Set `DJ_CONFIG_DIR` to use another
folder.

## Workspace

| Crate | What it does |
|---|---|
| `crates/engine` | Realtime audio engine: decks, key-lock time-stretch, mixer, insert FX chain, native effects, sampler, clock, limiter. No allocation or locking on the audio thread. |
| `crates/vst3host` | VST3 effect host (bundle loading, parameters, state, latency, process context). |
| `crates/library` | Decoding (symphonia), tag reading (ID3, FLAC/Ogg, MP4, Serato markers), waveforms, folder scanning, analysis of untagged tracks (BPM, grid, energy, cue points). |
| `crates/stems-core` | Stem separation: HT-Demucs and small ONNX models, offline over whole tracks. |
| `crates/app` | The application (`dj`): audio I/O (cpal), GUI (egui), stems, auto-mix, OSC, MIDI, sessions. |
| `crates/vst3-testplugin` | A tiny VST3 gain plugin used to test the host end to end. |
| `xtask` | `cargo xtask package-macos`: the `.app` and `.dmg`. |

```bash
cargo test --workspace
```

## Stems

Every track is separated into **drums, bass, vocals and other** with HT-Demucs, in the background
after it loads (`crates/stems-core`). The deck plays the full mix until the stems are ready; its
header shows the progress until they're there.

**Where stems come from**, in this order:

1. **Prepared stems**: made earlier by Demucs (on load, or with `dj --prepare-stems FOLDER` before
   a set) and kept in the *Prepared stems* folder (Settings → Stems; about 32 MB per minute, never
   cleaned up automatically). They're found by the track's audio, so retagging (Mixed In Key
   writing cues), renaming or moving a file keeps them.
2. **HT-Demucs**, the best quality: Settings → Stems → *Download HT-Demucs* (or
   `dj --download-demucs`) fetches the ONNX conversion made by the
   [Mixxx project](https://github.com/mixxxdj/demucs) (300 MB). About 1–3 minutes per track. Its
   weights are trained partly on non-commercial data, so dj downloads them on request and never
   ships them. HT-Demucs FT (four models, slightly better, 4× slower) works too: select any one of
   its four `name.<stem>.onnx` files.

Without either, a track has no stems (the deck says so); Mix's Auto then uses EQ, and cue points
are detected from the full mix.

**Playing with stems**: each stem's level can be put on its channel fader (Drums / Bass / Vocals /
Other, in the fader popup), and the popup's *Stems* group turns them all back to the plain mix. Stems are mixed where the deck reads audio, before key lock, loops, slip and hot
cues, so there is no added latency. MIDI and OSC: `Deck/N/StemDrums` … `StemOther` (switches),
`Deck/N/StemDrumsLevel` … (0–1), `Acapella`, `Instrumental`.

**Build.** The default `stems-onnx` feature downloads ONNX Runtime at build time and links it
statically. Where that download is blocked, build with
`--no-default-features --features stems-onnx-dynamic` and set `ORT_DYLIB_PATH` to an existing
`libonnxruntime` instead. stems-core's own tests: `cargo test -p stems-core --features onnx`.

## Mixed In Key

dj reads, from MP3, WAV, AIFF, FLAC, Ogg/Opus and M4A files:

- **BPM**: `TBPM`, `BPM`, `tmpo`.
- **Key**: `TKEY`, `INITIALKEY`, or iTunes `initialkey`, in Camelot (`8A`),
  Open Key (`1m`) or musical (`Am`) notation, or from an `8A - Energy 6`
  comment. You choose how keys are displayed.
- **Energy level**: `EnergyLevel`, or the comment.
- **Cue points and loops**: Serato Markers2 (MIK's "write cues to Serato"
  option). Hot cues keep their pad number, name and colour.
- **Beat grid**: the Serato BeatGrid if present. Otherwise the tagged BPM is
  anchored on the first cue. Only when there's no cue does dj look for
  where the kicks fall, still using the tagged BPM.

**Tracks without tags** (fresh downloads, promos) are analysed when they load
(`crates/library/src/analysis.rs`; keys are never guessed):

- **BPM and beat grid**: tempo from an onset envelope (kick-weighted spectral
  flux), folded into 78–178 BPM, rounded to a whole BPM when within measuring
  accuracy; for live drummers (tempo drifting between 30 s sections) the
  average, as Mixed In Key does. Bar starts from where the sound changes most.
  Shown as `≈124.00` on the deck.
- **Energy**: a rough 1–10 estimate from loudness, brightness and tempo.
- **Cue points**: section changes on phrase boundaries, named Intro, Drop,
  Break, Vocal, Outro or Phrase. With stems (Demucs), drums, bass and vocals are
  measured directly, so a drop is where drums and bass come in and a vocal cue
  where the singing starts; bars follow the actual beats (live tempo drift).
  Settings → Library → *Cue points*: Mixed In Key's (detect only for tracks
  without cues) or dj's detection for every track; the deck menu has *Detect
  cue points* for one track. Cues you set yourself are always kept.

Detected values are saved per track and shown in the library.

Grid nudges (`⊢`, `‹`, `›` on the deck) and cues you set while performing are
saved per track. Lock a track (🔒) to keep your prepared cues untouched.

Harmonic mixing: each deck's key is shown after pitch shifting, and coloured by
compatibility with the master deck (green: same, ±1 or relative; teal: +2
energy boost; red: clash). The browser can show only compatible tracks.

## Insert FX (§7.4), and what's improved

Each mixer channel has up to **4 inserts**. Open them with the channel's **FX**
button.

| | Syndicate | dj |
|---|---|---|
| Slots per channel, PRE/POST, ON, BYPASS ALL, drag to reorder | ✓ | ✓ |
| Signal-flow line and latency compensation | ✓ | ✓ (every channel and the cue bus are time-aligned) |
| 8 macros per channel, MAP learn | one parameter per macro | **up to 4 parameters per macro, each with its own min/max (invertible)** |
| Per-slot dry/wet mix | — | ✓ |
| Built-in effects | removed when VST is enabled | **always available**: Echo, Reverb, Flanger, Phaser, Auto Filter, Bitcrusher, Trance Gate, Beat Repeat, Distortion, Compressor, Utility (all tempo-synced to the master clock) |
| A broken plugin | "can stop the app" | Plugins are **scanned in a separate process**. An effect that panics, returns an error or outputs NaN/Inf is **bypassed and marked red**, not sent to the speakers. Re-enable it from the `⋯` menu. |
| Presets | encrypted `.sdfx` | **plain JSON** (whole chain incl. plugin state and macros) |
| Missing plugin | kept as missing | kept as missing |
| Bypass | click | **click-free ramps** |
| OSC | `/…/Mixer/CH{n}/FX`, `/FX/Macro/{1-8}` | same, plus `FX/Slot/{s}/On`, `FX/Slot/{s}/Mix`, `FX/Slot/{s}/Param/{k}`, `FX/Insert <id> [post]`, `FX/Slot/{s}/Remove` |

VST3 plugins are found in the standard locations (plus folders you add in
Settings), grouped by vendor and searchable. Parameters are edited in a generic
editor (every automatable parameter, with the plugin's own value text).

## Interface

One screen, laid out for 1600×1000 points and fitted to the window:

- **Waveform lanes** across the top for all four decks, **beat-aligned**: every
  lane uses the same pixels-per-beat, so synced decks line up vertically.
  Played audio is dimmed; stems are drawn in their colours; bar lines, named cue
  flags and loops are drawn on the lane, and a progress strip with cue ticks
  sits under each. Drag a lane to scrub, scroll to zoom, click the strip to jump.
- **Decks**: two large decks and two compact ones below (**Swap** brings a
  compact deck into the large view). With a MIDI controller connected
  (Menu → Layout: Auto / Touch / Controller), all four decks are full size with
  one slim row of cue pads.
  - Large: title/artist, key chip (green border = mixes with the master, red =
    clash), BPM (`≈` = detected; **MASTER** on the deck that sets the tempo,
    chosen automatically), remaining time; Play/Pause, Sync, and **Mix** when
    the deck is waiting to be mixed in; cue pads: the main cue first, then the
    hot cues that are set, showing their names (up to 9 pads); loop In/Out/Exit,
    ÷2, loop size, ×2, « ».
    A set hot cue pad loops from its cue at the loop size, or jumps to it
    (Settings → Hot cue pads); controller pads do the same. While another deck
    plays, a tapped cue turns Sync on, so it comes in in time.
  - Compact: Play/Pause, Cue, Sync, Swap (and Mix).
  - Right-click (or long-press) a deck's number for track/sampler mode, tempo range, lock, grid tools,
    *Detect cue points* and eject. Sampler decks show their step grid in the
    large view.
- **Mixer** in the middle: strips CH 3 · CH 1 · MASTER · CH 2 · CH 4. Each
  fader shows one function, named on the chip above it: Volume, High, Mid,
  Low, Filter, Drums, Bass, Vocals, Other or Tempo. Tap the chip for the
  fader popup: pick the function, or turn a whole group back to neutral (EQ,
  Stems, Tempo reset, Volume, Filter; ↺ restores). On a synced deck, Tempo
  reset resets the master's tempo, which the deck follows. Moving a mapped MIDI
  control switches that channel's fader to show it. Controls dj moves itself
  (these buttons, Mix) are picked up: a controller fader takes over again once
  moved to the new value, so nothing jumps. Muting is volume off: the
  fader drops to the bottom. CUE per strip, LIMIT on the master. Tempo fine
  steps: Settings.
- **Browser**: **Import folder** opens the system folder picker (every
  subfolder is included; on macOS this also grants access to an external
  drive); folders can also be typed in Settings → Library. Search, All tracks /
  History, **Fits**: tracks whose key mixes
  with the master deck and whose BPM is within ±5, best match first and
  focused; Quantize and Menu (settings, recording, layout); Title, Artist, BPM,
  Key (Camelot + musical), Match/Boost tag, Energy, Time, Genre; a deck badge marks loaded
  tracks; drag a row onto a deck or lane, or hover for load-to buttons.

Fonts: Inter and JetBrains Mono, bundled under the SIL Open Font License (see
`crates/app/assets/fonts`). The GUI is egui with custom-painted controls.

## Other features

- **Decks**: 4 decks (or 2). Key lock with a WSOLA time-stretcher and
  independent transpose. Tempo ranges ±6/10/16/50 % or ±12 semitones. Sync with
  continuous phase lock; the tempo master is always a playing deck, picked
  automatically (handoff when it stops or is ejected). Quantized launches (global or per deck), 8 hot cues
  (momentary, cue-to-next-cue loops), loops (in/out, halve/double, move,
  free-loop), slip mode, slip loops (with dotted/triplet), beat/bar skip,
  reverse, backspin, tape stop, half/double speed, phase nudge.
- **Mix**: while another deck plays, a deck that is paused (at its cue) or
  looping at a cue with its volume down gets a **Mix** button next to Sync.
  Mix syncs it (a paused deck is first turned down, looped where it stands and
  started in time) and asks only **Now** or **Best**. dj works out the rest:
  how (stems and EQ together, or EQ alone without stems; a cut only when the
  two can't blend), where and how long, measured as the best transition
  within the range (soon for Now, anywhere in the playing track for Best).
  - **When**: **Now** starts on the playing track's next phrase lines, with
    the new track's vocals (stems) out; they come back on its next phrase
    that has vocals. **Best** looks over the whole playing track.
    With less than a minute of the playing track left, Mix mixes now without
    asking.

  **Each mix is found, not scripted.** dj reads both tracks bar by bar: the
  level of drums, bass, vocals and other (from the stems; without stems, an
  estimate from the low/mid/high bands) and loudness, and from those their
  sections: intro (no bass yet), drops (bass back after 4+ bars), breaks,
  outro. It then tries every combination of where the old track can start
  leaving (its 4-bar lines, within 4 bars for *Now*), where the new track
  comes in (your cue, its start, its intro end, its drops and the phrases
  before them, its drop cue) and how long, and scores each bar of the
  overlap on what you'd hear: vocals or (clashing keys) melodies on top of
  each other, both tracks full at once, the old outro/break meeting the new
  intro; plus the new drop landing as the old track leaves, a natural bass
  swap, phrase lines, the loudness jump after the mix, and waiting.
  **How long comes from the songs:** every phrase length up to 64 bars is
  tried and every bar the two play well together counts, so the mix lasts as
  long as the music lets it (a long outro under a long intro: a long mix; two
  full tracks with vocals: a short one). The waveform lanes mark where the
  mix starts, ends and where the basses swap, and the reasons are printed
  in the terminal.

  **Gentle in, volume last out.** The new track's volume is fully up as it
  starts; its EQ (every band killed, then the highs rising from the first
  bar, then the mids, then the bass swap) or its stems (each part fading in
  over bars: the melody from the first bar, the drums later) bring it in.
  The old track leaves the same way, on its EQ or stems, and is nearly gone a
  bar before the end; only then does its volume close. The parts move where
  the music says (bass swap where a bass comes in or stops, parts that enter
  or stop by themselves left alone). **Techniques only when called for:** a
  high-pass sweep when the old track is still full as it has to go (EQ), an
  echo-out when it leaves mid-phrase with vocals or is cut while its bass
  plays, a loop of the old outro when it would end before the mix does.

  The new track always starts on its own bar line: if its cue isn't on a
  downbeat, it starts a few beats early so its bars fall on the playing
  track's. The tempo stays the playing track's.

  The old track stops at the end and the new one becomes the tempo master.
  Tap the button during a mix to cancel it. Controller faders that dj moved
  are picked up: they take effect again once moved to (or past) dj's value.
  **With an external mixer** dj can't see or move your faders: any deck
  looping at a cue while another plays gets Mix, volume and EQ are never
  touched, and *EQ* becomes *You fade*. Stems and Drop still blend on the
  decks. At the end the old deck drops into a loop, with its stems at 0 after
  Stems or Drop, and stays that way until you load another track or change
  it, so nothing comes back before you've faded it out.
- **Sampler decks**: 8 lanes, 16 steps, 8 patterns, per-step velocity, per-lane
  length (polyrhythms), direction (→ ← ↔ random), rate (normal/dotted/triplet),
  AHR envelope with curve, pitch, mono/poly, swing, global pitch, ½×/2×.
- **Mixer**: trim; 3-band full-kill EQ (LR4 crossovers, adjustable); one-knob
  CUTOFF/RES/WIDTH filter; fader curves (Linear dB / Constant power / Dipped /
  Fast cut); CUE/MUTE/SOLO; fader start. Outputs: master, none, or a direct
  hardware pair (choose which stages apply).
- **Master**: look-ahead limiter, master/cue levels, cue mix, split cue,
  recording to 32-bit float WAV.
- **Control**: MIDI learn (toggle/absolute/relative), LED feedback, MIDI clock
  out, keyboard shortcuts, OSC input + feedback.

## OSC API

UDP port 9090 by default. The address root defaults to `/dj/`; set it
to `/Syndicate/` in Settings to keep existing controller layouts. Addresses are
case-insensitive and bundles are accepted.

Examples:

```
/dj/Header/BPM f 126.0
/dj/Header/Quantize i 4            (0 off, 1 = 2 bars … 7 = 1/32)
/dj/Deck/1/PlayPause i 1
/dj/Deck/2/Load s /Volumes/SSD/Music/track.mp3
/dj/Deck/2/BeatSkip i -8
/dj/Deck/1/HotCue i 3 i 1 … i 3 i 0 (hold cue 3)
/dj/Deck/1/SlipLoop/Quarter i 1 … i 0
/dj/Deck/1/Sampler/LaneSet i 3 s Volume f 0.8
/dj/Mixer/CH3/Volume f 0.75
/dj/Mixer/CH1/FX/Macro/2 f 0.4
/dj/Mixer/CH1/FX/Insert s native:echo s post
/dj/Feedback/Resend i 1
```

Deck, sampler, mixer and header routes follow the original's §14 tables
(PlayPause, Playing, Cue, Cue1–8, SetCueLoop, TempoSync, TempoMaster,
TempoSlider, Loop, LoopIn/Out, SlipLoop/…, Skip…, PitchPreserve, Pitch…,
Phase…, WaveformSeek, LoopSize, Quantize, EQ/Low|Mid|High, Filter1–3, Mute/Solo/Cue,
Output, FX, FX/Macro/n, and more). Feedback (when enabled) sends BPM, Clock
beats, deck state, TrackInfo, Position/Progress/Time, LoopInfo, Cues,
CueFire, Transport and mixer values, only when they change.

The same addresses are used for MIDI and keyboard mappings. Add `:value` to fix
the value a button sends, e.g. `Deck/1/BeatSkip:-8`.

## Not included

- **Key detection.** By design: Mixed In Key provides keys.
- **Arrange mode, MIDI clips, OSC/automation clips.** Left out at your request.
- **Plugin GUIs.** VST3 effects are edited with the generic parameter editor.
- **VST3 instruments, Ableton Link, Syndicate NET, Rekordbox/Lexicon import,
  USB libraries, tag editor.**
- MIDI clock and OSC beat messages are sent from the UI thread, so they jitter
  by a few milliseconds and aren't shifted for plugin latency.
- Changing the audio device needs a restart.

Used on macOS (Apple silicon) with a Xone:K2; the test suite runs on macOS and Windows (CI) and
Linux. The Windows build hasn't been tried on a Windows machine with audio hardware yet.
