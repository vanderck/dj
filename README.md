# Collective

Live-performance DJ software written entirely in Rust. It's modelled on the
feature set of Untitled Audio Syndicate's *Syndicate*: four decks, a full mixer
with **VST3 channel inserts** (User Guide §7.4), sampler decks, a shared master
clock, and MIDI/OSC control. It has its own name and code.

It's built for a **Mixed In Key** workflow. Collective doesn't detect BPM, key
or cue points; it reads what Mixed In Key wrote into your files.

## Build & run

Requires Rust 1.80+ (stable).

```bash
cargo build --release
```

```bash
./target/release/collective              # GUI
./target/release/collective --headless   # engine + OSC/MIDI only (no window)
./target/release/collective --info track.mp3   # show the tags Collective reads
./target/release/collective --list-devices
```

Linux needs the ALSA development headers to build (`libasound2-dev` on
Debian/Ubuntu). The GUI uses wgpu and falls back to OpenGL automatically; set
`COLLECTIVE_RENDERER=glow` to force OpenGL.

Settings, session, mappings, FX presets and per-track edits are stored as
readable JSON in your config directory (`~/.config/collective` on Linux,
`~/Library/Application Support/collective` on macOS,
`%APPDATA%\collective` on Windows). Set `COLLECTIVE_CONFIG_DIR` to use another
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

Every track is separated into **drums, bass, vocals and other** in the background after it loads,
using [`stems-core`](https://github.com/vanderck/stems). The deck plays the full mix until the stems
are ready (a progress bar under the waveform shows how far it is), then each deck gets:

- **DRUMS / BASS / VOCALS / OTHER** toggles (lit = audible) and **ACAPELLA / INSTR** shortcuts.
- Zero added latency: stems are mixed where the deck reads audio, before key lock, loops, slip and
  hot cues, so all of those keep working.
- MIDI learn and OSC: `/Collective/Deck/N/StemDrums`, `StemBass`, `StemVocals`, `StemOther`,
  `Acapella`, `Instrumental` (switches, 1 = on), with feedback.

Separated stems are cached (about 50 MB per minute of audio; size limit and *Clear cache* in
Settings → Stems), so reloading a track is instant.

**Model.** Set an ONNX separation model in Settings → Stems (or the `STEMS_MODEL` environment
variable). Without one, a low-quality placeholder separator is used and the deck says so in red.
Models are trained with the pipeline in the stems repository.

**Build.** The default `stems-onnx` feature downloads ONNX Runtime at build time and links it
statically. Where that download is blocked, build with
`--no-default-features --features stems-onnx-dynamic` and set `ORT_DYLIB_PATH` to an existing
`libonnxruntime` instead.

## Mixed In Key

Collective reads, from MP3, WAV, AIFF, FLAC, Ogg/Opus and M4A files:

- **BPM**: `TBPM`, `BPM`, `tmpo`.
- **Key**: `TKEY`, `INITIALKEY`, or iTunes `initialkey`, in Camelot (`8A`),
  Open Key (`1m`) or musical (`Am`) notation, or from an `8A - Energy 6`
  comment. You choose how keys are displayed.
- **Energy level**: `EnergyLevel`, or the comment.
- **Cue points and loops**: Serato Markers2 (MIK's "write cues to Serato"
  option). Hot cues keep their pad number, name and colour.
- **Beat grid**: the Serato BeatGrid if present. Otherwise the tagged BPM is
  anchored on the first cue. Only when there's no cue does Collective look for
  where the kicks fall, still using the tagged BPM.

Grid nudges (`⊢`, `‹`, `›` on the deck) and cues you set while performing are
saved per track. Lock a track (🔒) to keep your prepared cues untouched.

Harmonic mixing: each deck's key is shown after pitch shifting, and coloured by
compatibility with the master deck (green: same, ±1 or relative; teal: +2
energy boost; red: clash). The browser can show only compatible tracks.

## Insert FX (§7.4), and what's improved

Each mixer channel has up to **4 inserts**. Open them with the channel's **FX**
button.

| | Syndicate | Collective |
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

The Perform view mirrors the familiar 4-deck layout: decks 1 and 3 on the
left, 2 and 4 on the right, channel strips in the middle, browser below.

- **Deck header:** SYNC, deck badge (right-click: track/sampler, lock, grid
  tools), title / artist / length, BAR/BEAT, REMAIN, ORIGINAL BPM, CURRENT BPM,
  KEY (coloured on the Camelot wheel, transposed with pitch), QUANTIZE (click to
  change), eject.
- **Second row:** MASTER/FOLLOW, P (key lock), − 0 + transpose, a BEND strip on
  the master deck or an EARLY ▼ LATE phase strip on followers, M (momentary
  cues), /2, X2, SLIP, direction.
- **Body:** tempo fader and play button on the left; zoomed and overview
  waveforms coloured by cue section; 8 named cue pads; loop, I/O, skip, slip
  loops 1/32–1/2 bar, D/T.
- **Channel:** big volume knob with a dotted VU ring and trim ring
  (Ctrl/Cmd-drag), F for fader start, FILTER (CUTOFF/RES/WIDTH; click the title
  for EQ), CUE/MUTE/SOLO/FX, output.
- **Browser:** ALL/HIST, search, harmonic filter, IMPORT, row zoom; ARTIST,
  TITLE, ALBUM, LABEL, BPM, DURATION, KEY, GENRE, PLAY COUNT, DATE ADDED. Drag
  rows onto a deck or double-click to load.

The GUI is egui (immediate mode, custom-painted controls), not a game engine:
it redraws only what's needed, starts instantly and keeps the audio thread
completely separate.

## Other features

- **Decks**: 4 decks (or 2). Key lock with a WSOLA time-stretcher and
  independent transpose. Tempo ranges ±6/10/16/50 % or ±12 semitones. Sync with
  continuous phase lock; MASTER/FOLLOW clock ownership with automatic handoff;
  wallclock lock. Quantized launches (global or per deck), 8 hot cues
  (momentary, cue-to-next-cue loops), loops (in/out, halve/double, move,
  free-loop), slip mode, slip loops (with dotted/triplet), beat/bar skip,
  reverse, backspin, tape stop, half/double speed, phase nudge.
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

UDP port 9090 by default. The address root defaults to `/Collective/`; set it
to `/Syndicate/` in Settings to keep existing controller layouts. Addresses are
case-insensitive and bundles are accepted.

Examples:

```
/Collective/Header/BPM f 126.0
/Collective/Header/Quantize i 4            (0 off, 1 = 2 bars … 7 = 1/32)
/Collective/Deck/1/PlayPause i 1
/Collective/Deck/2/BeatSkip i -8
/Collective/Deck/1/HotCue i 3 i 1 … i 3 i 0 (hold cue 3)
/Collective/Deck/1/SlipLoop/Quarter i 1 … i 0
/Collective/Deck/1/Sampler/LaneSet i 3 s Volume f 0.8
/Collective/Mixer/CH3/Volume f 0.75
/Collective/Mixer/CH1/FX/Macro/2 f 0.4
/Collective/Mixer/CH1/FX/Insert s native:echo s post
/Collective/Feedback/Resend i 1
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
