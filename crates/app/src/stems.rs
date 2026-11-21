//! Stem separation in the background, with HT-Demucs: tracks are separated after they load (the
//! deck plays the plain mix meanwhile) and kept as prepared stems; the engine receives them through
//! `Command::AttachStems`. Without prepared stems or a Demucs model a track has no stems.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::SystemTime;

use dj_engine::engine::NUM_DECKS;
use dj_engine::track::{Cue, StemData, Track};
use crossbeam_channel::{unbounded, Receiver, Sender};
use stems_core::offline::{self, OfflineError, StemBuffers};
use stems_core::Stem as CoreStem;

pub enum Update {
    Progress { deck: usize, track_id: u64, fraction: f32 },
    /// `cues`: cue points detected with the stems, when the request asked for them.
    Done { deck: usize, track_id: u64, stems: Arc<StemData>, wave: Arc<StemWave>, source: String, cues: Option<Vec<Cue>> },
    /// No stems for this track: none prepared and no HT-Demucs model, or it failed (`error`, to
    /// report). `cues`: cue points detected from the mix instead, when the request asked for them.
    NoStems { deck: usize, track_id: u64, error: Option<String>, cues: Option<Vec<Cue>> },
}

pub struct Request {
    pub deck: usize,
    pub track: Arc<Track>,
    pub path: PathBuf,
    /// HT-Demucs ONNX model (see [`demucs_files`]); results are kept as prepared stems.
    pub demucs: Option<PathBuf>,
    /// Detect cue points with the stems once they are ready (see `Update::Done`).
    pub detect_cues: bool,
    /// Folder of prepared stems (from Demucs, on load or with `--prepare-stems`); checked
    /// first and never evicted.
    pub prepared: Option<PathBuf>,
}

/// Separator id of prepared stems in cache keys: one set per track, whichever command made it.
const PREPARED: &str = "prepared";

pub struct StemsWorker {
    /// A Demucs model download started from the settings, if any.
    pub demucs_download: Option<Download>,
    jobs: Sender<Request>,
    updates: Receiver<Update>,
    /// Track id each deck currently wants stems for (0 = none). Jobs for anything else are
    /// skipped or cancelled mid-way.
    wanted: Arc<[AtomicU64; NUM_DECKS]>,
}

impl StemsWorker {
    pub fn new() -> Self {
        let (jobs, job_rx) = unbounded::<Request>();
        let (tx, updates) = unbounded();
        let wanted: Arc<[AtomicU64; NUM_DECKS]> = Arc::new(std::array::from_fn(|_| AtomicU64::new(0)));
        let w = wanted.clone();
        std::thread::Builder::new()
            .name("stems".into())
            .spawn(move || {
                let mut demucs = DemucsCache::default();
                while let Ok(job) = job_rx.recv() {
                    run(job, &w, &tx, &mut demucs);
                }
            })
            .expect("spawn stems worker");
        Self { demucs_download: None, jobs, updates, wanted }
    }

    pub fn request(&self, req: Request) {
        if req.deck < NUM_DECKS {
            self.wanted[req.deck].store(req.track.id, Ordering::Relaxed);
            let _ = self.jobs.send(req);
        }
    }

    /// Stop working on (or skip) whatever this deck asked for.
    pub fn cancel(&self, deck: usize) {
        if let Some(w) = self.wanted.get(deck) {
            w.store(0, Ordering::Relaxed);
        }
    }

    pub fn try_recv(&self) -> Option<Update> {
        self.updates.try_recv().ok()
    }
}

/// Per-stem loudness for the waveform lanes: peak level per bin, at the same rate as the track's
/// waveform, scaled so 1.0 is the loudest moment of the full mix.
pub struct StemWave {
    pub bins_per_sec: f64,
    /// In `Stem` order: drums, bass, vocals, other.
    pub levels: [Vec<f32>; 4],
}

impl StemWave {
    pub fn compute(mix: &[[f32; 2]], sample_rate: u32, stems: &StemData) -> Self {
        let bins_per_sec = dj_library::waveform::BINS_PER_SEC;
        let per = (sample_rate as f64 / bins_per_sec).max(1.0);
        let bins = (mix.len() as f64 / per).ceil() as usize;
        let mut levels: [Vec<f32>; 4] = std::array::from_fn(|_| vec![0.0; bins]);
        let mut mix_peak = 1e-6f32;
        let deq = |v: [i16; 2]| [v[0] as f32 * stems.scale, v[1] as f32 * stems.scale];
        for (i, m) in mix.iter().enumerate() {
            let b = ((i as f64 / per) as usize).min(bins - 1);
            let get = |s: &Vec<[i16; 2]>| s.get(i).copied().map(deq).unwrap_or([0.0; 2]);
            let (d, ba, v) = (get(&stems.drums), get(&stems.bass), get(&stems.vocals));
            let o = [m[0] - d[0] - ba[0] - v[0], m[1] - d[1] - ba[1] - v[1]];
            for (k, f) in [d, ba, v, o].iter().enumerate() {
                let a = f[0].abs().max(f[1].abs());
                if a > levels[k][b] {
                    levels[k][b] = a;
                }
            }
            mix_peak = mix_peak.max(m[0].abs().max(m[1].abs()));
        }
        for l in &mut levels {
            for x in l.iter_mut() {
                *x = (*x / mix_peak).min(1.0);
            }
        }
        Self { bins_per_sec, levels }
    }
}

/// Report finished stems, with cue points detected from them when asked for.
fn done(job: &Request, tx: &Sender<Update>, stems: StemData, source: String) {
    let audio = &job.track.audio;
    let wave = Arc::new(StemWave::compute(&audio.frames, audio.sample_rate, &stems));
    let cues = match (job.detect_cues, job.track.grid) {
        (true, Some(grid)) => {
            let env = dj_library::analysis::Envelope::compute(audio);
            Some(dj_library::analysis::detect_cues(audio, &env, &grid, Some(&stems)))
        }
        _ => None,
    };
    let _ = tx.send(Update::Done { deck: job.deck, track_id: job.track.id, stems: Arc::new(stems), wave, source, cues });
}

fn run(job: Request, wanted: &[AtomicU64; NUM_DECKS], tx: &Sender<Update>, demucs: &mut DemucsCache) {
    let (deck, id) = (job.deck, job.track.id);
    let still_wanted = || wanted[deck].load(Ordering::Relaxed) == id;
    if !still_wanted() {
        return;
    }
    let audio = &job.track.audio;
    let prepared_key = content_key(&audio.frames, audio.sample_rate);
    if let Some(dir) = &job.prepared {
        if let Some(stems) = find_prepared(dir, &job.path, &audio.frames, audio.sample_rate, id) {
            done(&job, tx, stems, "prepared stems".into());
            return;
        }
    }
    let mut last = -1.0f32;
    let mut progress = |p: f32| {
        if p - last >= 0.02 {
            last = p;
            let _ = tx.send(Update::Progress { deck, track_id: id, fraction: p });
        }
        still_wanted()
    };
    // Prepared-quality separators: their stems are kept for good.
    let deliver_prepared = |buffers: StemBuffers, name: String| {
        let stems = StemData::from_f32(
            id,
            &buffers[CoreStem::Drums as usize],
            &buffers[CoreStem::Bass as usize],
            &buffers[CoreStem::Vocals as usize],
        );
        if let Some(dir) = &job.prepared {
            if let Err(e) = cache::write(dir, prepared_key, &stems) {
                eprintln!("prepared stems: {e}");
            }
        }
        done(&job, tx, stems, name);
    };
    let threads = std::thread::available_parallelism().map(|n| n.get().saturating_sub(2).max(1)).unwrap_or(1);
    let error = match &job.demucs {
        Some(path) => match demucs.separate(path, threads, &audio.frames, audio.sample_rate, &mut progress) {
            Ok((buffers, name)) => return deliver_prepared(buffers, name),
            Err(OfflineError::Cancelled) => return,
            Err(e) => Some(format!("Demucs: {e}")),
        },
        None => None,
    };
    no_stems(&job, tx, error);
}

/// No stems for this track: say so, with cue points from the mix when they were asked for.
fn no_stems(job: &Request, tx: &Sender<Update>, error: Option<String>) {
    let audio = &job.track.audio;
    let cues = match (job.detect_cues, job.track.grid) {
        (true, Some(grid)) => {
            let env = dj_library::analysis::Envelope::compute(audio);
            Some(dj_library::analysis::detect_cues(audio, &env, &grid, None))
        }
        _ => None,
    };
    let _ = tx.send(Update::NoStems { deck: job.deck, track_id: job.track.id, error, cues });
}

fn mtime_secs(meta: Option<&fs::Metadata>) -> u64 {
    meta.and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Key of a track's prepared stems: its decoded audio (length, rate and a sample every few
/// thousand frames), so tag edits (Mixed In Key writing cues), renames and moves keep them.
fn content_key(frames: &[[f32; 2]], rate: u32) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |bytes: &[u8]| {
        for b in bytes {
            h ^= *b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    eat(b"audio-1");
    eat(&(frames.len() as u64).to_le_bytes());
    eat(&rate.to_le_bytes());
    let step = (frames.len() / 4096).max(1);
    for f in frames.iter().step_by(step) {
        eat(&f[0].to_bits().to_le_bytes());
        eat(&f[1].to_bits().to_le_bytes());
    }
    h
}

/// A track's prepared stems: by its audio, else (stems made by older versions) by the file's
/// identity, which then move to the audio key.
fn find_prepared(dir: &Path, path: &Path, frames: &[[f32; 2]], rate: u32, track_id: u64) -> Option<StemData> {
    let n = frames.len();
    let key = content_key(frames, rate);
    if let Some(s) = cache::read(&dir.join(cache::file_name(key)), key, track_id, n) {
        return Some(s);
    }
    let old = cache_key(path, PREPARED, n);
    let old_file = dir.join(cache::file_name(old));
    let stems = cache::read(&old_file, old, track_id, n)?;
    if cache::write(dir, key, &stems).is_ok() {
        let _ = fs::remove_file(&old_file);
    }
    Some(stems)
}

/// Stable 64-bit FNV-1a over the track file identity and the separator (how prepared stems were
/// keyed before [`content_key`]).
fn cache_key(path: &Path, separator: &str, frames: usize) -> u64 {
    let meta = fs::metadata(path).ok();
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |bytes: &[u8]| {
        for b in bytes {
            h ^= *b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    eat(path.to_string_lossy().as_bytes());
    eat(&meta.as_ref().map(|m| m.len()).unwrap_or(0).to_le_bytes());
    eat(&mtime_secs(meta.as_ref()).to_le_bytes());
    eat(separator.as_bytes());
    eat(&(frames as u64).to_le_bytes());
    h
}

/// HT-Demucs model files for a path: one file, or for a bag of models (HT-Demucs FT exported
/// as `name.drums.onnx`, `name.bass.onnx`, `name.other.onnx`, `name.vocals.onnx`) any one of the
/// four, which brings in its siblings.
pub fn demucs_files(path: &Path) -> Vec<PathBuf> {
    const STEMS: [&str; 4] = ["drums", "bass", "other", "vocals"];
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    for stem in STEMS {
        if let Some(base) = name.strip_suffix(&format!(".{stem}.onnx")) {
            return STEMS.iter().map(|s| path.with_file_name(format!("{base}.{s}.onnx"))).collect();
        }
    }
    vec![path.to_path_buf()]
}

/// The loaded Demucs model, kept between tracks (loading takes a second or two).
#[derive(Default)]
pub struct DemucsCache {
    #[cfg(any(feature = "stems-onnx", feature = "stems-onnx-dynamic"))]
    loaded: Option<(Vec<PathBuf>, usize, stems_core::demucs::DemucsModel)>,
}

impl DemucsCache {
    #[cfg(any(feature = "stems-onnx", feature = "stems-onnx-dynamic"))]
    pub fn separate(
        &mut self,
        path: &Path,
        threads: usize,
        frames: &[[f32; 2]],
        rate: u32,
        progress: offline::Progress,
    ) -> Result<(StemBuffers, String), OfflineError> {
        use stems_core::demucs::DemucsModel;
        let files = demucs_files(path);
        if !matches!(&self.loaded, Some((f, t, _)) if *f == files && *t == threads) {
            self.loaded = None; // free the old model before loading another
            let options = stems_core::OnnxOptions { intra_threads: threads };
            let model = DemucsModel::from_files(&files, &options).map_err(|e| OfflineError::Model(e.to_string()))?;
            self.loaded = Some((files, threads, model));
        }
        let (_, _, model) = self.loaded.as_mut().unwrap();
        let stems = model.separate(frames, rate, progress)?;
        Ok((stems, model.name().to_string()))
    }

    #[cfg(not(any(feature = "stems-onnx", feature = "stems-onnx-dynamic")))]
    pub fn separate(
        &mut self,
        path: &Path,
        _: usize,
        _: &[[f32; 2]],
        _: u32,
        _: offline::Progress,
    ) -> Result<(StemBuffers, String), OfflineError> {
        Err(OfflineError::Model(format!("cannot use {}: built without ONNX support", path.display())))
    }
}

/// Where dj keeps a downloaded HT-Demucs model.
pub fn default_demucs_path() -> PathBuf {
    crate::settings::config_dir().join("models").join("htdemucs.onnx")
}

/// HT-Demucs as exported to ONNX by the Mixxx project (MIT code; the weights are Meta's,
/// trained partly on MUSDB18, so for non-commercial use). Downloaded on request, never bundled.
pub const DEMUCS_URL: &str = "https://github.com/mixxxdj/demucs/releases/latest/download/htdemucs.onnx";

#[derive(Clone, Debug, PartialEq)]
pub enum DownloadState {
    Running { done: u64, total: Option<u64> },
    Done(PathBuf),
    Failed(String),
}

/// A download running on its own thread.
pub struct Download {
    pub state: Arc<std::sync::Mutex<DownloadState>>,
}

impl Download {
    pub fn start(url: &str, dest: PathBuf) -> Self {
        let state = Arc::new(std::sync::Mutex::new(DownloadState::Running { done: 0, total: None }));
        let s = state.clone();
        let url = url.to_string();
        std::thread::spawn(move || {
            let result = download(&url, &dest, &mut |done, total| {
                *s.lock().unwrap() = DownloadState::Running { done, total };
            });
            *s.lock().unwrap() = match result {
                Ok(()) => DownloadState::Done(dest),
                Err(e) => DownloadState::Failed(e),
            };
        });
        Self { state }
    }

    pub fn state(&self) -> DownloadState {
        self.state.lock().unwrap().clone()
    }
}

/// Download `url` to `dest` (via a temporary file, so a broken download never looks complete).
pub fn download(url: &str, dest: &Path, progress: &mut dyn FnMut(u64, Option<u64>)) -> Result<(), String> {
    use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
    // The operating system's TLS and certificate store (macOS keychain).
    let tls = TlsConfig::builder().provider(TlsProvider::NativeTls).root_certs(RootCerts::PlatformVerifier).build();
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .tls_config(tls)
        .build()
        .into();
    let mut resp = agent.get(url).call().map_err(|e| format!("{url}: {e}"))?;
    let total = resp
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if let Some(dir) = dest.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let tmp = dest.with_extension("part");
    let mut file = std::io::BufWriter::new(fs::File::create(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?);
    let mut reader = resp.body_mut().with_config().limit(u64::MAX).reader();
    let mut buf = vec![0u8; 1 << 16];
    let mut done = 0u64;
    loop {
        let n = reader.read(&mut buf).map_err(|e| format!("downloading: {e}"))?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n]).map_err(|e| format!("{}: {e}", tmp.display()))?;
        done += n as u64;
        progress(done, total);
    }
    file.flush().map_err(|e| format!("{}: {e}", tmp.display()))?;
    drop(file);
    if total.is_some_and(|t| t != done) {
        let _ = fs::remove_file(&tmp);
        return Err(format!("download incomplete ({done} of {} bytes)", total.unwrap()));
    }
    fs::rename(&tmp, dest).map_err(|e| format!("{}: {e}", dest.display()))
}

/// What [`prepare_file`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum Prepared {
    Done(String),
    AlreadyPrepared,
}

/// Separate one file with Demucs into the prepared-stems folder `dir`, unless it is already
/// there (for `dj --prepare-stems`). `progress` gets 0..=1.
pub fn prepare_file(
    path: &Path,
    model: &Path,
    threads: usize,
    cache: &mut DemucsCache,
    dir: &Path,
    progress: &mut dyn FnMut(f32),
) -> Result<Prepared, String> {
    let audio = dj_library::decode(path).map_err(|e| format!("decoding: {e}"))?;
    let key = content_key(&audio.frames, audio.sample_rate);
    if find_prepared(dir, path, &audio.frames, audio.sample_rate, 0).is_some() {
        return Ok(Prepared::AlreadyPrepared);
    }
    let (buffers, name) = cache
        .separate(model, threads, &audio.frames, audio.sample_rate, &mut |p| {
            progress(p);
            true
        })
        .map_err(|e| e.to_string())?;
    let stems = StemData::from_f32(
        0,
        &buffers[CoreStem::Drums as usize],
        &buffers[CoreStem::Bass as usize],
        &buffers[CoreStem::Vocals as usize],
    );
    cache::write(dir, key, &stems).map_err(|e| format!("writing {}: {e}", dir.display()))?;
    Ok(Prepared::Done(name))
}

mod cache {
    use super::*;

    const MAGIC: &[u8; 8] = b"CSTEMS01";

    pub fn file_name(key: u64) -> String {
        format!("{key:016x}.stems")
    }

    pub fn read(path: &Path, key: u64, track_id: u64, frames: usize) -> Option<StemData> {
        let mut f = fs::File::open(path).ok()?;
        let mut header = [0u8; 28];
        f.read_exact(&mut header).ok()?;
        if &header[..8] != MAGIC
            || u64::from_le_bytes(header[8..16].try_into().ok()?) != key
            || u64::from_le_bytes(header[16..24].try_into().ok()?) != frames as u64
        {
            return None;
        }
        let scale = f32::from_le_bytes(header[24..28].try_into().ok()?);
        let mut bytes = Vec::with_capacity(frames * 12);
        f.read_to_end(&mut bytes).ok()?;
        if bytes.len() != frames * 12 {
            return None;
        }
        let stem = |k: usize| -> Vec<[i16; 2]> {
            bytes[k * frames * 4..(k + 1) * frames * 4]
                .chunks_exact(4)
                .map(|c| [i16::from_le_bytes([c[0], c[1]]), i16::from_le_bytes([c[2], c[3]])])
                .collect()
        };
        Some(StemData { track_id, scale, drums: stem(0), bass: stem(1), vocals: stem(2) })
    }

    pub fn write(dir: &Path, key: u64, stems: &StemData) -> std::io::Result<()> {
        fs::create_dir_all(dir)?;
        let path = dir.join(file_name(key));
        let tmp = path.with_extension("tmp");
        {
            let mut f = std::io::BufWriter::new(fs::File::create(&tmp)?);
            f.write_all(MAGIC)?;
            f.write_all(&key.to_le_bytes())?;
            f.write_all(&(stems.len() as u64).to_le_bytes())?;
            f.write_all(&stems.scale.to_le_bytes())?;
            for s in [&stems.drums, &stems.bass, &stems.vocals] {
                for fr in s.iter() {
                    f.write_all(&fr[0].to_le_bytes())?;
                    f.write_all(&fr[1].to_le_bytes())?;
                }
            }
            f.flush()?;
        }
        fs::rename(&tmp, &path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dj_engine::track::AudioData;
    use std::time::Duration;

    fn track(id: u64, secs: f32) -> Arc<Track> {
        let rate = 44_100;
        let frames = (0..(secs * rate as f32) as usize)
            .map(|i| {
                let t = i as f32 / rate as f32;
                let v = 0.4 * (std::f32::consts::TAU * 60.0 * t).sin() + 0.2 * (std::f32::consts::TAU * 900.0 * t).sin();
                [v, v]
            })
            .collect();
        Arc::new(Track {
            id,
            audio: AudioData { sample_rate: rate, frames },
            grid: None,
            tempo_map: None,
            gain: 1.0,
            cues: Default::default(),
        })
    }

    fn wait(w: &StemsWorker) -> Update {
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        loop {
            match w.try_recv() {
                Some(Update::Progress { .. }) => {}
                Some(u) => return u,
                None if std::time::Instant::now() > deadline => panic!("timed out"),
                None => std::thread::sleep(Duration::from_millis(5)),
            }
        }
    }

    /// The tiny stand-in HT-Demucs model from stems-core's tests.
    #[cfg(any(feature = "stems-onnx", feature = "stems-onnx-dynamic"))]
    fn test_model() -> PathBuf {
        PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../stems-core/tests/data/demucs_test.onnx"))
    }

    fn req(track: Arc<Track>, demucs: Option<PathBuf>, detect_cues: bool, prepared: Option<PathBuf>) -> Request {
        Request { deck: 0, track, path: PathBuf::from("/nonexistent/song.wav"), demucs, detect_cues, prepared }
    }

    #[test]
    fn without_demucs_there_are_no_stems_but_cues_still_come() {
        let mut t = Arc::try_unwrap(track(21, 40.0)).ok().unwrap();
        t.grid = Some(dj_engine::track::BeatGrid { bpm: 120.0, anchor_secs: 0.0 });
        let t = Arc::new(t);
        let w = StemsWorker::new();
        w.request(req(t.clone(), None, true, None));
        let Update::NoStems { track_id: 21, error: None, cues: Some(_), .. } = wait(&w) else { panic!("expected no stems, with cues from the mix") };
        w.request(req(t, None, false, None));
        let Update::NoStems { cues: None, .. } = wait(&w) else { panic!() };
    }

    #[cfg(any(feature = "stems-onnx", feature = "stems-onnx-dynamic"))]
    #[test]
    fn cancelled_jobs_produce_nothing() {
        let w = StemsWorker::new();
        w.request(req(track(31, 30.0), Some(test_model()), false, None));
        w.cancel(0);
        std::thread::sleep(Duration::from_millis(300));
        while let Some(u) = w.try_recv() {
            assert!(matches!(u, Update::Progress { .. }), "a cancelled job delivers nothing");
        }
    }

    #[test]
    fn demucs_ft_files_bring_their_siblings() {
        let one = Path::new("/m/htdemucs.onnx");
        assert_eq!(demucs_files(one), vec![one.to_path_buf()]);
        let ft = demucs_files(Path::new("/m/htdemucs_ft.vocals.onnx"));
        assert_eq!(ft.len(), 4);
        assert_eq!(ft[0], Path::new("/m/htdemucs_ft.drums.onnx"));
        assert_eq!(ft[3], Path::new("/m/htdemucs_ft.vocals.onnx"));
    }

    /// Needs ONNX Runtime at run time (with `stems-onnx-dynamic`, set ORT_DYLIB_PATH).
    #[cfg(any(feature = "stems-onnx", feature = "stems-onnx-dynamic"))]
    #[test]
    fn demucs_stems_become_prepared_stems() {
        let dir = std::env::temp_dir().join(format!("dj-demucs-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let model = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../stems-core/tests/data/demucs_test.onnx"));
        let t = track(61, 1.0);
        let req = |track, demucs: Option<PathBuf>| Request {
            deck: 2,
            track,
            path: dir.join("song.wav"),
            demucs,
            detect_cues: false,
            prepared: Some(dir.join("prepared")),
        };
        let w = StemsWorker::new();
        w.request(req(t.clone(), Some(model.clone())));
        let Update::Done { stems, source, .. } = wait(&w) else { panic!("expected Demucs stems") };
        assert_eq!(source, "HT-Demucs");
        // The stand-in model makes vocals 0.4 x (mix - mean) + mean.
        let m = t.audio.frames.iter().map(|f| 0.5 * (f[0] + f[1])).sum::<f32>() / t.audio.frames.len() as f32;
        let i = 12_345;
        let want = 0.4 * (t.audio.frames[i][0] - m) + m;
        assert!((stems.vocals[i][0] as f32 * stems.scale - want).abs() < 1e-3);
        w.request(req(track(62, 1.0), Some(model)));
        let Update::Done { track_id: 62, source, .. } = wait(&w) else { panic!() };
        assert_eq!(source, "prepared stems");

        // A missing model is reported: no stems.
        let _ = fs::remove_dir_all(dir.join("prepared"));
        w.request(req(track(63, 1.0), Some(dir.join("missing.onnx"))));
        let Update::NoStems { track_id: 63, error: Some(error), .. } = wait(&w) else { panic!("expected the Demucs error") };
        assert!(error.contains("Demucs"), "{error}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(any(feature = "stems-onnx", feature = "stems-onnx-dynamic"))]
    #[test]
    fn prepare_file_separates_once() {
        let dir = std::env::temp_dir().join(format!("dj-prepare-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let song = dir.join("song.wav");
        let t = track(71, 1.0);
        let spec = hound::WavSpec { channels: 2, sample_rate: t.audio.sample_rate, bits_per_sample: 32, sample_format: hound::SampleFormat::Float };
        let mut w = hound::WavWriter::create(&song, spec).unwrap();
        for f in &t.audio.frames {
            w.write_sample(f[0]).unwrap();
            w.write_sample(f[1]).unwrap();
        }
        w.finalize().unwrap();
        let model = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../stems-core/tests/data/demucs_test.onnx"));
        let mut cache = DemucsCache::default();
        let mut prepare = || prepare_file(&song, model, 2, &mut cache, &dir.join("prepared"), &mut |_| {});
        assert_eq!(prepare(), Ok(Prepared::Done("HT-Demucs".into())));
        assert_eq!(prepare(), Ok(Prepared::AlreadyPrepared));
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(any(feature = "stems-onnx", feature = "stems-onnx-dynamic"))]
    #[test]
    fn detects_cue_points_with_the_stems_when_asked() {
        let mut t = Arc::try_unwrap(track(81, 40.0)).ok().unwrap();
        t.grid = Some(dj_engine::track::BeatGrid { bpm: 120.0, anchor_secs: 0.0 });
        let t = Arc::new(t);
        let w = StemsWorker::new();
        w.request(req(t.clone(), Some(test_model()), true, None));
        let Update::Done { cues, .. } = wait(&w) else { panic!() };
        assert!(cues.is_some(), "cue detection ran");
        w.request(req(t, Some(test_model()), false, None));
        let Update::Done { cues, .. } = wait(&w) else { panic!() };
        assert!(cues.is_none());
    }
}
