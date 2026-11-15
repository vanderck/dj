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
}

pub struct StemsWorker {
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
                while let Ok(job) = job_rx.recv() {
                    run(job, &w, &tx);
                }
            })
            .expect("spawn stems worker");
        Self { jobs, updates, wanted }
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

fn run(job: Request, wanted: &[AtomicU64; NUM_DECKS], tx: &Sender<Update>) {
    let (deck, id) = (job.deck, job.track.id);
    let still_wanted = || wanted[deck].load(Ordering::Relaxed) == id;
    if !still_wanted() {
        return;
    }
    let audio = &job.track.audio;
    let separator = separator_id(job.model.as_deref());
    let key = cache_key(&job.path, &separator, audio.frames.len());

    if let Some((dir, _)) = &job.cache {
        if let Some(stems) = cache::read(&dir.join(cache::file_name(key)), key, id, audio.frames.len()) {
            let wave = Arc::new(StemWave::compute(&audio.frames, audio.sample_rate, &stems));
            let _ = tx.send(Update::Done { deck, track_id: id, stems: Arc::new(stems), wave, source: "cache".into() });
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
        w.request(Request { deck: 1, track: t.clone(), path: song.clone(), model: None, cache: cache.clone() });
        let Update::Done { deck, track_id, stems, source } = wait(&w) else { panic!("expected stems") };
        assert_eq!((deck, track_id, source.as_str()), (1, 11, "placeholder separator"));
        assert_eq!(stems.len(), t.audio.frames.len());
        // Unity gains reproduce the mix; all three stored stems plus "other" sum back to it.
        let i = 30_000;
        let mix = stems.mix(i, t.audio.frames[i], &[1.0; 4]);
        assert!((mix[0] - t.audio.frames[i][0]).abs() < 1e-6);

        // Same file again (new track id): served from the cache, identical content.
        let t2 = track(12, 2.0);
        w.request(Request { deck: 0, track: t2, path: song.clone(), model: None, cache: cache.clone() });
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
        w.request(Request { deck: 2, track: track(21, 30.0), path: PathBuf::from("/nonexistent"), model: None, cache: None });
        w.cancel(2);
        // A later request for another deck still completes, and nothing arrives for the cancelled one.
        w.request(Request { deck: 3, track: track(22, 1.0), path: PathBuf::from("/nonexistent"), model: None, cache: None });
        let Update::Done { deck, track_id, .. } = wait(&w) else { panic!() };
        assert_eq!((deck, track_id), (3, 22));
    }

    #[test]
    fn model_errors_are_reported_then_fall_back_to_the_placeholder() {
        let w = StemsWorker::new();
        let bad = std::env::temp_dir().join("collective-not-a-model.onnx");
        fs::write(&bad, b"nope").unwrap();
        w.request(Request { deck: 0, track: track(31, 1.0), path: bad.clone(), model: Some(bad), cache: None });
        let Update::Failed { track_id: 31, error, .. } = wait(&w) else { panic!("expected the model error first") };
        assert!(error.contains("placeholder"), "{error}");
        let Update::Done { track_id: 31, source, .. } = wait(&w) else { panic!("expected placeholder stems") };
        assert_eq!(source, "placeholder separator");
    }
}
