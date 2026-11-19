# dj

Live-performance DJ software written entirely in Rust. It's modelled on the
feature set of Untitled Audio Syndicate's *Syndicate*: four decks, a full mixer
with **VST3 channel inserts** (User Guide §7.4), sampler decks, a shared master
clock, and MIDI/OSC control. It has its own name and code. (It was called Collective; the
first start moves the old `collective` settings folder to `dj`, and the old `/Collective/` OSC
prefix becomes `/dj/`.)

It's built for a **Mixed In Key** workflow. dj doesn't detect BPM, key
or cue points; it reads what Mixed In Key wrote into your files.

## Build & run

Requires Rust 1.80+ (stable).

```bash
cargo build --release
```

```bash
./target/release/dj              # GUI
./target/release/dj --headless   # engine + OSC/MIDI only (no window)
./target/release/dj --info track.mp3   # show the tags dj reads
./target/release/dj --list-devices
```

macOS app bundle and disk image (see [packaging/macos](packaging/macos/README.md)):

```bash
cargo xtask package-macos --universal
```

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
| `crates/library` | Decoding (symphonia), tag reading (ID3, FLAC/Ogg, MP4, Serato markers), waveforms, folder scanning. |
| `crates/app` | The application: audio I/O (cpal), GUI (egui), OSC, MIDI, sessions. |
| `crates/vst3-testplugin` | A tiny VST3 gain plugin used to test the host end to end. |

```bash
cargo test --workspace
```

## Stems

Every track is separated into **drums, bass, vocals and other** in the background after it loads
(`crates/stems-core`). The deck plays the full mix until the stems are ready; its header shows the
progress and, afterwards, which separator made them.

**Separators**, tried in this order:

1. **Prepared stems**: made earlier by Demucs (on load, or with `dj --prepare-stems FOLDER` before
   a set) and kept in the *Prepared stems* folder (Settings → Stems; about 32 MB per minute, never
   cleaned up automatically).
2. **HT-Demucs**, the best quality: Settings → Stems → *Download HT-Demucs* (or
   `dj --download-demucs`) fetches the ONNX conversion made by the
   [Mixxx project](https://github.com/mixxxdj/demucs) (300 MB). About 1–3 minutes per track. Its
   weights are trained partly on non-commercial data, so dj downloads them on request and never
   ships them. HT-Demucs FT (four models, slightly better, 4× slower) works too: select any one of
   its four `name.<stem>.onnx` files.
3. **A small model** (Settings → Stems → Model, or `STEMS_MODEL`): seconds per track, more bleed.
   See [docs/stems/MODEL_CONTRACT.md](docs/stems/MODEL_CONTRACT.md).
4. Without any of these, a low-quality placeholder, and the deck says so in red.

Results of 3 and 4 are cached (size limit and *Clear cache* in Settings → Stems).

**Playing with stems**: each stem has a level on the channel faders (Drums / Bass / Vocals / Other,
with ON/OFF). Stems are mixed where the deck reads audio, before key lock, loops, slip and hot
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

One screen, scaled to the window (1600×1000 design baseline; Settings → Scale
adjusts it further):

- **Waveform lanes** across the top for all four decks, **beat-aligned**: every
  lane uses the same pixels-per-beat, so synced decks line up vertically.
  Played audio is dimmed; bar lines, named cue flags and loops are drawn on the
  lane, and a progress strip with cue ticks sits under each. Drag a lane to
  scrub, scroll to zoom, click the strip to jump.
- **Two large decks** (left and right) and **two compact decks** below them.
  **Swap** brings a compact deck into the large view.
  - Large: title/artist, Camelot key chip (green border = mixes with the
    master, red = clash), BPM and pitch %, remaining time; Cue (hold),
    Play/Pause, Sync, Master/Follow, Key lock, Slip, Reverse (Shift-click:
    backspin), Roll; 8 named cue pads; tempo slider (right-click: range);
    In / Out / ÷2 / loop size / ×2 / « » jump; Hi / Mid / Low / Filter (click a
    label to kill/reset). With **Roll** on, the loop-size button rolls while
    held.
  - Right-click a deck's number for track/sampler mode, lock, grid tools and
    eject. Sampler decks show their step grid in the large view.
- **Mixer** in the middle: tempo (drag or double-click), beat dots, Tap,
  Quantize (right-click for the value), Rec, Menu; strips ordered
  CH 3 · CH 1 · MASTER · CH 2 · CH 4 with level in dB, CUE (LIMIT on master),
  fader with live meter (Ctrl/Cmd-drag: trim), and − FX +; right-click a strip
  title for mute, solo, trim, filter resonance/width and output; crossfader
  (decks 1/3 left, 2/4 right).
- **Browser**: search, All tracks / History, *Fits deck N (key)* harmonic
  filter, track stats, Import folder; Title, Artist, BPM, Key (Camelot +
  musical), Match/Boost tag, Energy, Time, Genre; a deck badge marks loaded
  tracks; hover a row for load-to buttons, or drag it onto a deck or lane.

Fonts: Inter and JetBrains Mono, bundled under the SIL Open Font License (see
`crates/app/assets/fonts`). The GUI is egui with custom-painted controls.

## Other features

- **Decks**: 4 decks (or 2). Key lock with a WSOLA time-stretcher and
  independent transpose. Tempo ranges ±6/10/16/50 % or ±12 semitones. Sync with
  continuous phase lock; MASTER/FOLLOW clock ownership with automatic handoff;
  wallclock lock. Quantized launches (global or per deck), 8 hot cues
  (momentary, cue-to-next-cue loops), loops (in/out, halve/double, move,
  free-loop), slip mode, slip loops (with dotted/triplet), beat/bar skip,
  reverse, backspin, tape stop, half/double speed, phase nudge.
- **Mix**: tap a cue on a silent deck (volume down) while another deck plays:
  it loops from the cue, and a **Mix** button appears next to Sync. Mix syncs
  it and asks *Now* or *Best*. **Now** starts on the next bar and mixes over the
  whole bars that fit in about 10 s, with the new track's vocals (stems) out
  until the old track is gone. **Best** waits for an 8-bar phrase start and
  mixes over 16 bars, picking the moment where the two tracks' vocals clash
  least (from the stems), preferring the playing track's outro or break. With
  less than a minute of the playing track left, Mix mixes now without asking.
  Either way the new track comes up over the first half with its bass out, the
  basses swap at the midpoint, the old track fades over the second half and
  stops, and the new one becomes the tempo master. Tap the button during a mix
  to cancel it. Controller faders that dj moved are picked up: they take
  effect again once moved to (or past) dj's value.
- **Sampler decks**: 8 lanes, 16 steps, 8 patterns, per-step velocity, per-lane
  length (polyrhythms), direction (→ ← ↔ random), rate (normal/dotted/triplet),
  AHR envelope with curve, pitch, mono/poly, swing, global pitch, ½×/2×.
- **Mixer**: trim; 3-band full-kill EQ (LR4 crossovers, adjustable); one-knob
  CUTOFF/RES/WIDTH filter; fader curves (Linear dB / Constant power / Dipped /
  Fast cut); CUE/MUTE/SOLO; fader start. Outputs: master, none, or a direct
  hardware pair (choose which stages apply). Crossfader with A/B/thru assign
  on any channel, or a 4-deck vector pad.
- **Master**: look-ahead limiter, master/cue levels, cue mix, split cue,
  metronome, recording to 32-bit float WAV.
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
Phase…, WaveformSeek, Quantize, EQ/Low|Mid|High, Filter1–3, Mute/Solo/Cue,
Output, FX, FX/Macro/n, and more). Feedback (when enabled) sends BPM, Clock
beats, deck state, TrackInfo, Position/Progress/Time, LoopInfo, Cues,
CueFire, Transport and mixer values, only when they change.

The same addresses are used for MIDI and keyboard mappings. Add `:value` to fix
the value a button sends, e.g. `Deck/1/BeatSkip:-8`.

## Not included

- **BPM / key / cue detection.** By design: Mixed In Key provides them.
- **Arrange mode, MIDI clips, OSC/automation clips.** Left out at your request.
- **Plugin GUIs.** VST3 effects are edited with the generic parameter editor.
- **VST3 instruments, Ableton Link, Syndicate NET, Rekordbox/Lexicon import,
  USB libraries, tag editor.**
- MIDI clock and OSC beat messages are sent from the UI thread, so they jitter
  by a few milliseconds and aren't shifted for plugin latency.
- Changing the audio device needs a restart.

Tested on Linux (headless and under a virtual X display). The macOS and
Windows code paths (VST3 bundle entry points, audio, GUI) compile from the same
sources but haven't been run here.
