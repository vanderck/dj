//! Stem separation in the background: tracks are separated after they load (the deck plays the
//! plain mix meanwhile), results are cached on disk, and the engine receives them through
//! `Command::AttachStems`. Separation itself is `stems_core::offline`.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::SystemTime;

use collective_engine::engine::NUM_DECKS;
use collective_engine::track::{StemData, Track};
use crossbeam_channel::{unbounded, Receiver, Sender};
use stems_core::offline::{self, OfflineError, StemBuffers};
use stems_core::Stem as CoreStem;

pub enum Update {
    Progress { deck: usize, track_id: u64, fraction: f32 },
    Done { deck: usize, track_id: u64, stems: Arc<StemData>, wave: Arc<StemWave>, source: String },
    Failed { deck: usize, track_id: u64, error: String },
}

pub struct Request {
    pub deck: usize,
    pub track: Arc<Track>,
    pub path: PathBuf,
    /// ONNX model; `None` uses the placeholder separator.
    pub model: Option<PathBuf>,
    /// Cache folder and its size limit in bytes; `None` disables the cache.
    pub cache: Option<(PathBuf, u64)>,
    /// HT-Demucs ONNX model (see [`demucs_files`]); tried first, results kept as prepared stems.
    pub demucs: Option<PathBuf>,
    /// External separator command (see [`external`]); tried before `model`.
    pub external: Option<String>,
    /// Folder of prepared stems (from the external separator or `--prepare-stems`); checked
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
        let bins_per_sec = collective_library::waveform::BINS_PER_SEC;
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

/// Model from the settings, else `STEMS_MODEL`, else one bundled with the app.
pub fn resolve_model(setting: Option<&Path>) -> Option<PathBuf> {
    setting
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("STEMS_MODEL").filter(|v| !v.is_empty()).map(PathBuf::from))
        .or_else(bundled_model)
}

/// File name of a model shipped with the app (see `cargo xtask package-macos --model`).
pub const BUNDLED_MODEL: &str = "stems-model.onnx";

/// `Collective.app/Contents/Resources/stems-model.onnx` on macOS, or next to the executable.
pub fn bundled_model() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    [dir.join("../Resources").join(BUNDLED_MODEL), dir.join(BUNDLED_MODEL)].into_iter().find(|p| p.is_file())
}

fn run(job: Request, wanted: &[AtomicU64; NUM_DECKS], tx: &Sender<Update>, demucs: &mut DemucsCache) {
    let (deck, id) = (job.deck, job.track.id);
    let still_wanted = || wanted[deck].load(Ordering::Relaxed) == id;
    if !still_wanted() {
        return;
    }
    let audio = &job.track.audio;
    let frames = audio.frames.len();
    let prepared_key = cache_key(&job.path, PREPARED, frames);
    if let Some(dir) = &job.prepared {
        if let Some(stems) = cache::read(&dir.join(cache::file_name(prepared_key)), prepared_key, id, frames) {
            let wave = Arc::new(StemWave::compute(&audio.frames, audio.sample_rate, &stems));
            let _ = tx.send(Update::Done { deck, track_id: id, stems: Arc::new(stems), wave, source: "prepared stems".into() });
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
            if let Err(e) = cache::write(dir, prepared_key, &stems, u64::MAX) {
                eprintln!("prepared stems: {e}");
            }
        }
        let wave = Arc::new(StemWave::compute(&audio.frames, audio.sample_rate, &stems));
        let _ = tx.send(Update::Done { deck, track_id: id, stems: Arc::new(stems), wave, source: name });
    };
    let threads = std::thread::available_parallelism().map(|n| n.get().saturating_sub(2).max(1)).unwrap_or(1);
    if let Some(path) = &job.demucs {
        match demucs.separate(path, threads, &audio.frames, audio.sample_rate, &mut progress) {
            Ok((buffers, name)) => return deliver_prepared(buffers, name),
            Err(OfflineError::Cancelled) => return,
            Err(e) => {
                let error = format!("Demucs: {e}; using the next separator");
                let _ = tx.send(Update::Failed { deck, track_id: id, error });
            }
        }
    }
    if let Some(command) = &job.external {
        match external::run(&audio.frames, audio.sample_rate, command, &mut progress) {
            Ok((buffers, name)) => return deliver_prepared(buffers, name),
            Err(external::Error::Cancelled) => return,
            Err(external::Error::Failed(e)) => {
                let error = format!("external separator failed ({e}); using the built-in model");
                let _ = tx.send(Update::Failed { deck, track_id: id, error });
            }
        }
    }

    let separator = separator_id(job.model.as_deref());
    let key = cache_key(&job.path, &separator, frames);
    if let Some((dir, _)) = &job.cache {
        if let Some(stems) = cache::read(&dir.join(cache::file_name(key)), key, id, audio.frames.len()) {
            let wave = Arc::new(StemWave::compute(&audio.frames, audio.sample_rate, &stems));
            let _ = tx.send(Update::Done { deck, track_id: id, stems: Arc::new(stems), wave, source: "cache".into() });
            return;
        }
    }
    let mut result = separate(&audio.frames, audio.sample_rate, job.model.as_deref(), &mut progress);
    if let (Err(OfflineError::Model(e)), Some(model)) = (&result, &job.model) {
        // Better rough stems than none: report the problem, then use the placeholder.
        let error = format!("{}: {e}; using the placeholder separator", model.display());
        let _ = tx.send(Update::Failed { deck, track_id: id, error });
        result = separate(&audio.frames, audio.sample_rate, None, &mut progress);
    }
    match result {
        Ok((buffers, source)) => {
            // "Other" is not stored: the engine derives it as mix − drums − bass − vocals.
            let stems = StemData::from_f32(
                id,
                &buffers[CoreStem::Drums as usize],
                &buffers[CoreStem::Bass as usize],
                &buffers[CoreStem::Vocals as usize],
            );
            drop(buffers);
            if let Some((dir, limit)) = &job.cache {
                if let Err(e) = cache::write(dir, key, &stems, *limit) {
                    eprintln!("stems cache: {e}");
                }
            }
            let wave = Arc::new(StemWave::compute(&audio.frames, audio.sample_rate, &stems));
            let _ = tx.send(Update::Done { deck, track_id: id, stems: Arc::new(stems), wave, source });
        }
        Err(OfflineError::Cancelled) => {}
        Err(e) => {
            let _ = tx.send(Update::Failed { deck, track_id: id, error: e.to_string() });
        }
    }
}

fn separate(
    frames: &[[f32; 2]],
    rate: u32,
    model: Option<&Path>,
    progress: offline::Progress,
) -> Result<(StemBuffers, String), OfflineError> {
    match model {
        None => Ok((offline::separate_with_heuristic(frames, rate, progress)?, "placeholder separator".into())),
        Some(path) => separate_with_onnx(frames, rate, path, progress),
    }
}

#[cfg(any(feature = "stems-onnx", feature = "stems-onnx-dynamic"))]
fn separate_with_onnx(
    frames: &[[f32; 2]],
    rate: u32,
    path: &Path,
    progress: offline::Progress,
) -> Result<(StemBuffers, String), OfflineError> {
    use stems_core::{MaskModel, OnnxModel, OnnxOptions};
    // Separation runs in the background: leave cores for the audio thread and the UI.
    let threads = std::thread::available_parallelism().map(|n| n.get().saturating_sub(2).max(1)).unwrap_or(1);
    let model = OnnxModel::from_file(path, &OnnxOptions { intra_threads: threads })
        .map_err(|e| OfflineError::Model(e.to_string()))?;
    let name = model.name().to_string();
    Ok((offline::separate_with_model(frames, rate, model, progress)?, name))
}

#[cfg(not(any(feature = "stems-onnx", feature = "stems-onnx-dynamic")))]
fn separate_with_onnx(
    _: &[[f32; 2]],
    _: u32,
    path: &Path,
    _: offline::Progress,
) -> Result<(StemBuffers, String), OfflineError> {
    Err(OfflineError::Model(format!(
        "cannot use {}: built without ONNX support (enable the `stems-onnx` feature)",
        path.display()
    )))
}

/// Identifies the separator, so cached stems are recomputed when the model file changes.
fn separator_id(model: Option<&Path>) -> String {
    match model {
        None => "heuristic-1".into(),
        Some(p) => {
            let meta = fs::metadata(p).ok();
            let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
            format!("{}:{size}:{}", p.display(), mtime_secs(meta.as_ref()))
        }
    }
}

fn mtime_secs(meta: Option<&fs::Metadata>) -> u64 {
    meta.and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Stable 64-bit FNV-1a over the track file identity and the separator.
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

/// Where Collective keeps a downloaded HT-Demucs model.
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

/// Separate one file with the external separator into the prepared-stems folder, unless it is
/// already there (for `collective --prepare-stems`). `progress` gets 0..=1.
/// The separator `prepare_file` uses.
pub enum Preparer<'a> {
    Demucs { model: &'a Path, threads: usize, cache: &'a mut DemucsCache },
    External(&'a str),
}

pub fn prepare_file(path: &Path, with: &mut Preparer, dir: &Path, progress: &mut dyn FnMut(f32)) -> Result<Prepared, String> {
    let audio = collective_library::decode(path).map_err(|e| format!("decoding: {e}"))?;
    let frames = audio.frames.len();
    let key = cache_key(path, PREPARED, frames);
    if cache::read(&dir.join(cache::file_name(key)), key, 0, frames).is_some() {
        return Ok(Prepared::AlreadyPrepared);
    }
    let mut keep_going = |p| {
        progress(p);
        true
    };
    let (buffers, name) = match with {
        Preparer::Demucs { model, threads, cache } => cache
            .separate(model, *threads, &audio.frames, audio.sample_rate, &mut keep_going)
            .map_err(|e| e.to_string())?,
        Preparer::External(command) => {
            external::run(&audio.frames, audio.sample_rate, command, &mut keep_going).map_err(|e| match e {
                external::Error::Failed(e) => e,
                external::Error::Cancelled => "cancelled".into(),
            })?
        }
    };
    let stems = StemData::from_f32(
        0,
        &buffers[CoreStem::Drums as usize],
        &buffers[CoreStem::Bass as usize],
        &buffers[CoreStem::Vocals as usize],
    );
    cache::write(dir, key, &stems, u64::MAX).map_err(|e| format!("writing {}: {e}", dir.display()))?;
    Ok(Prepared::Done(name))
}

/// External separator: any program that turns a WAV file into stem files, so separators that
/// cannot ship with Collective (licence, size, Python) plug in from outside.
///
/// Collective runs `sh -c '<command> "$@"' collective INPUT.wav OUTPUT_DIR`, where INPUT.wav is
/// the decoded track (32-bit float, stereo, at the track's own sample rate). The program writes
/// `OUTPUT_DIR/{drums,bass,vocals,other}.wav` (any sample rate, mono or stereo, PCM or float;
/// `other` is optional: Collective derives it) and exits with status 0. It may print lines
/// `progress 0.42` and `name Some Separator` on stdout.
pub mod external {
    use super::*;
    use std::collections::VecDeque;
    use std::io::BufRead;
    use std::process::{Command, Stdio};
    use std::sync::Mutex;
    use std::time::Duration;

    pub enum Error {
        /// `progress` returned false; the program was stopped.
        Cancelled,
        Failed(String),
    }

    /// Removes the exchange folder however `run` ends.
    struct TempDir(PathBuf);
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    pub fn run(
        frames: &[[f32; 2]],
        rate: u32,
        command: &str,
        progress: &mut dyn FnMut(f32) -> bool,
    ) -> Result<(StemBuffers, String), Error> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let fail = |e: String| Error::Failed(e);
        let tmp = TempDir(std::env::temp_dir().join(format!(
            "collective-stems-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )));
        let out_dir = tmp.0.join("stems");
        fs::create_dir_all(&out_dir).map_err(|e| fail(format!("temp folder: {e}")))?;
        let input = tmp.0.join("input.wav");
        write_wav(&input, frames, rate).map_err(|e| fail(format!("writing input: {e}")))?;

        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("{command} \"$@\""))
            .arg("collective")
            .arg(&input)
            .arg(&out_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| fail(format!("starting it: {e}")))?;

        let (line_tx, lines) = crossbeam_channel::unbounded::<String>();
        let stdout = child.stdout.take().unwrap();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout).lines().map_while(Result::ok) {
                let _ = line_tx.send(line);
            }
        });
        let tail = Arc::new(Mutex::new(VecDeque::<String>::new()));
        let stderr = child.stderr.take().unwrap();
        let t = tail.clone();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stderr).lines().map_while(Result::ok) {
                let mut t = t.lock().unwrap();
                t.push_back(line);
                if t.len() > 8 {
                    t.pop_front();
                }
            }
        });

        let mut name = String::from("external separator");
        let mut fraction = 0.0f32;
        let status = loop {
            while let Ok(line) = lines.try_recv() {
                if let Some(p) = line.strip_prefix("progress ").and_then(|v| v.trim().parse::<f32>().ok()) {
                    fraction = p.clamp(0.0, 1.0);
                } else if let Some(n) = line.strip_prefix("name ") {
                    name = n.trim().to_string();
                }
            }
            if !progress(0.98 * fraction) {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Error::Cancelled);
            }
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                Err(e) => return Err(fail(format!("waiting for it: {e}"))),
            }
        };
        if !status.success() {
            std::thread::sleep(Duration::from_millis(50)); // let stderr drain
            let tail = tail.lock().unwrap().iter().cloned().collect::<Vec<_>>().join(" | ");
            return Err(fail(format!("{status}: {tail}")));
        }

        let mut stems: StemBuffers = Default::default();
        for s in [CoreStem::Drums, CoreStem::Bass, CoreStem::Vocals] {
            let path = out_dir.join(format!("{}.wav", s.name().to_lowercase()));
            let (mut audio, r) = read_wav(&path).map_err(|e| fail(format!("{}: {e}", path.display())))?;
            if r != rate {
                audio = stems_core::resample(&audio, r, rate);
            }
            audio.resize(frames.len(), [0.0; 2]);
            stems[s as usize] = audio;
        }
        Ok((stems, name))
    }

    fn write_wav(path: &Path, frames: &[[f32; 2]], rate: u32) -> Result<(), hound::Error> {
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: rate,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut w = hound::WavWriter::create(path, spec)?;
        for f in frames {
            w.write_sample(f[0])?;
            w.write_sample(f[1])?;
        }
        w.finalize()
    }

    /// Any PCM or float WAV -> stereo frames (mono duplicated, extra channels dropped).
    pub(super) fn read_wav(path: &Path) -> Result<(Vec<[f32; 2]>, u32), hound::Error> {
        let mut r = hound::WavReader::open(path)?;
        let spec = r.spec();
        let ch = spec.channels.max(1) as usize;
        let samples: Vec<f32> = match spec.sample_format {
            hound::SampleFormat::Float => r.samples::<f32>().collect::<Result<_, _>>()?,
            hound::SampleFormat::Int => {
                let k = 1.0 / (1u64 << (spec.bits_per_sample - 1)) as f32;
                r.samples::<i32>().map(|v| v.map(|v| v as f32 * k)).collect::<Result<_, _>>()?
            }
        };
        let frames = samples
            .chunks_exact(ch)
            .map(|c| if ch == 1 { [c[0], c[0]] } else { [c[0], c[1]] })
            .collect();
        Ok((frames, spec.sample_rate))
    }
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
        // Touch so eviction treats it as recently used.
        let _ = fs::File::options().append(true).open(path).and_then(|f| f.set_modified(SystemTime::now()));
        Some(StemData { track_id, scale, drums: stem(0), bass: stem(1), vocals: stem(2) })
    }

    pub fn write(dir: &Path, key: u64, stems: &StemData, limit: u64) -> std::io::Result<()> {
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
        fs::rename(&tmp, &path)?;
        evict(dir, limit);
        Ok(())
    }

    /// Delete least recently used files until the folder fits in `limit` bytes.
    pub fn evict(dir: &Path, limit: u64) {
        let Ok(rd) = fs::read_dir(dir) else { return };
        let mut files: Vec<(SystemTime, u64, PathBuf)> = rd
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "stems"))
            .filter_map(|e| {
                let m = e.metadata().ok()?;
                Some((m.modified().ok()?, m.len(), e.path()))
            })
            .collect();
        let mut total: u64 = files.iter().map(|f| f.1).sum();
        files.sort_by_key(|f| f.0); // oldest first
        for (_, len, path) in files {
            if total <= limit {
                break;
            }
            if fs::remove_file(&path).is_ok() {
                total -= len;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use collective_engine::track::AudioData;
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

    #[test]
    fn separates_caches_and_reloads_from_cache() {
        let dir = std::env::temp_dir().join(format!("collective-stems-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let song = dir.join("song.wav");
        fs::write(&song, b"pretend audio file").unwrap();
        let cache = Some((dir.join("cache"), 1 << 30));

        let w = StemsWorker::new();
        let t = track(11, 2.0);
        w.request(Request { deck: 1, track: t.clone(), path: song.clone(), model: None, cache: cache.clone(), demucs: None, external: None, prepared: None });
        let Update::Done { deck, track_id, stems, source, .. } = wait(&w) else { panic!("expected stems") };
        assert_eq!((deck, track_id, source.as_str()), (1, 11, "placeholder separator"));
        assert_eq!(stems.len(), t.audio.frames.len());
        // Unity gains reproduce the mix; all three stored stems plus "other" sum back to it.
        let i = 30_000;
        let mix = stems.mix(i, t.audio.frames[i], &[1.0; 4]);
        assert!((mix[0] - t.audio.frames[i][0]).abs() < 1e-6);

        // Same file again (new track id): served from the cache, identical content.
        let t2 = track(12, 2.0);
        w.request(Request { deck: 0, track: t2, path: song.clone(), model: None, cache: cache.clone(), demucs: None, external: None, prepared: None });
        let Update::Done { track_id, stems: cached, source, .. } = wait(&w) else { panic!() };
        assert_eq!((track_id, source.as_str()), (12, "cache"));
        assert_eq!(cached.drums, stems.drums);
        assert_eq!(cached.vocals, stems.vocals);
        assert_eq!(cached.scale, stems.scale);

        // A tiny limit evicts everything but what fits.
        cache::evict(&dir.join("cache"), 0);
        assert_eq!(fs::read_dir(dir.join("cache")).unwrap().count(), 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn cancelled_jobs_produce_nothing() {
        let w = StemsWorker::new();
        w.request(Request { deck: 2, track: track(21, 30.0), path: PathBuf::from("/nonexistent"), model: None, cache: None, demucs: None, external: None, prepared: None });
        w.cancel(2);
        // A later request for another deck still completes, and nothing arrives for the cancelled one.
        w.request(Request { deck: 3, track: track(22, 1.0), path: PathBuf::from("/nonexistent"), model: None, cache: None, demucs: None, external: None, prepared: None });
        let Update::Done { deck, track_id, .. } = wait(&w) else { panic!() };
        assert_eq!((deck, track_id), (3, 22));
    }

    #[test]
    fn model_errors_are_reported_then_fall_back_to_the_placeholder() {
        let w = StemsWorker::new();
        let bad = std::env::temp_dir().join("collective-not-a-model.onnx");
        fs::write(&bad, b"nope").unwrap();
        w.request(Request { deck: 0, track: track(31, 1.0), path: bad.clone(), model: Some(bad), cache: None, demucs: None, external: None, prepared: None });
        let Update::Failed { track_id: 31, error, .. } = wait(&w) else { panic!("expected the model error first") };
        assert!(error.contains("placeholder"), "{error}");
        let Update::Done { track_id: 31, source, .. } = wait(&w) else { panic!("expected placeholder stems") };
        assert_eq!(source, "placeholder separator");
    }

    /// A stand-in external separator: every stem is a copy of the input; counts its runs.
    fn fake_separator(dir: &Path) -> String {
        let script = dir.join("fake.sh");
        fs::write(
            &script,
            "echo 'name Fake separator'\necho 'progress 0.5'\nfor s in drums bass vocals; do cp \"$1\" \"$2/$s.wav\"; done\necho run >> \"$(dirname \"$0\")/runs\"\n",
        )
        .unwrap();
        format!("sh '{}'", script.display())
    }

    fn write_song(path: &Path, t: &Track) {
        let spec = hound::WavSpec { channels: 2, sample_rate: t.audio.sample_rate, bits_per_sample: 32, sample_format: hound::SampleFormat::Float };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        for f in &t.audio.frames {
            w.write_sample(f[0]).unwrap();
            w.write_sample(f[1]).unwrap();
        }
        w.finalize().unwrap();
    }

    #[test]
    fn external_separator_results_are_kept_as_prepared_stems() {
        let dir = std::env::temp_dir().join(format!("collective-ext-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let song = dir.join("song.wav");
        let t = track(41, 2.0);
        write_song(&song, &t);
        let decoded = |id| {
            let audio = collective_library::decode(&song).unwrap();
            Arc::new(Track { id, audio, grid: None, gain: 1.0, cues: Default::default() })
        };
        let t = decoded(41);
        let command = fake_separator(&dir);
        let prepared = Some(dir.join("prepared"));

        let w = StemsWorker::new();
        let req = |_id: u64, track: Arc<Track>| Request {
            deck: 0,
            track,
            path: song.clone(),
            model: None,
            cache: None,
            demucs: None,
            external: Some(command.clone()),
            prepared: prepared.clone(),
        };
        w.request(req(41, t.clone()));
        let Update::Done { stems, source, .. } = wait(&w) else { panic!("expected stems") };
        assert_eq!(source, "Fake separator");
        let i = 20_000;
        let back = stems.drums[i][0] as f32 * stems.scale;
        assert!((back - t.audio.frames[i][0]).abs() < 1e-3, "{back} vs {}", t.audio.frames[i][0]);

        // Loading the same file again uses the prepared stems without running the separator.
        w.request(req(42, decoded(42)));
        let Update::Done { track_id: 42, source, .. } = wait(&w) else { panic!() };
        assert_eq!(source, "prepared stems");
        assert_eq!(fs::read_to_string(dir.join("runs")).unwrap().lines().count(), 1);

        // `--prepare-stems` sees the same store.
        let mut p = |_| {};
        assert_eq!(prepare_file(&song, &mut Preparer::External(&command), &dir.join("prepared"), &mut p), Ok(Prepared::AlreadyPrepared));
        let other = dir.join("other.wav");
        write_song(&other, &track(43, 1.0));
        assert_eq!(prepare_file(&other, &mut Preparer::External(&command), &dir.join("prepared"), &mut p), Ok(Prepared::Done("Fake separator".into())));
        assert_eq!(fs::read_dir(dir.join("prepared")).unwrap().count(), 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn external_separator_failure_is_reported_then_falls_back() {
        let w = StemsWorker::new();
        let command = "echo 'no GPU today' >&2; exit 3; true".to_string();
        w.request(Request {
            deck: 1,
            track: track(51, 1.0),
            path: PathBuf::from("/nonexistent"),
            model: None,
            cache: None,
            demucs: None,
            external: Some(command),
            prepared: None,
        });
        let Update::Failed { track_id: 51, error, .. } = wait(&w) else { panic!("expected the failure first") };
        assert!(error.contains("no GPU today") && error.contains('3'), "{error}");
        let Update::Done { track_id: 51, source, .. } = wait(&w) else { panic!("expected fallback stems") };
        assert_eq!(source, "placeholder separator");
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
        let dir = std::env::temp_dir().join(format!("collective-demucs-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let model = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/demucs_test.onnx"));
        let t = track(61, 1.0);
        let req = |track, demucs: Option<PathBuf>| Request {
            deck: 2,
            track,
            path: dir.join("song.wav"),
            model: None,
            cache: None,
            demucs,
            external: None,
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

        // A missing model is reported and the next separator takes over.
        let _ = fs::remove_dir_all(dir.join("prepared"));
        w.request(req(track(63, 1.0), Some(dir.join("missing.onnx"))));
        let Update::Failed { track_id: 63, error, .. } = wait(&w) else { panic!("expected the Demucs error") };
        assert!(error.contains("Demucs"), "{error}");
        let Update::Done { track_id: 63, source, .. } = wait(&w) else { panic!() };
        assert_eq!(source, "placeholder separator");
        let _ = fs::remove_dir_all(&dir);
    }
}
