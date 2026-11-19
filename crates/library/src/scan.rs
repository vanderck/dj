//! Folder scanning for the browser (tags only; no decoding).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::key::Key;
use crate::tags;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Entry {
    pub path: PathBuf,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub genre: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub duration_secs: Option<f64>,
    /// When the file first appeared in the library (unix seconds).
    #[serde(default)]
    pub added: u64,
    pub bpm: Option<f64>,
    pub key: Option<Key>,
    pub energy: Option<u8>,
    pub cue_count: usize,
    pub modified: u64,
}

impl Entry {
    pub fn read(path: &Path) -> Entry {
        let t = tags::read(path).unwrap_or_default();
        let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        Entry {
            path: path.to_path_buf(),
            title: t.title.unwrap_or(stem),
            artist: t.artist.unwrap_or_default(),
            album: t.album.unwrap_or_default(),
            genre: t.genre.unwrap_or_default(),
            label: t.label.unwrap_or_default(),
            duration_secs: crate::probe_duration(path),
            added: now(),
            bpm: t.bpm,
            key: t.key,
            energy: t.energy,
            cue_count: t.cues.len(),
            modified: mtime(path),
        }
    }

    pub fn matches(&self, query: &str) -> bool {
        if query.is_empty() {
            return true;
        }
        let q = query.to_lowercase();
        q.split_whitespace().all(|w| {
            self.title.to_lowercase().contains(w)
                || self.artist.to_lowercase().contains(w)
                || self.album.to_lowercase().contains(w)
                || self.genre.to_lowercase().contains(w)
                || self.label.to_lowercase().contains(w)
                || self.key.map(|k| k.camelot().eq_ignore_ascii_case(w) || k.musical().eq_ignore_ascii_case(w)).unwrap_or(false)
        })
    }
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn mtime(p: &Path) -> u64 {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Recursively list audio files.
pub fn list_audio_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            let hidden = p.file_name().map(|n| n.to_string_lossy().starts_with('.')).unwrap_or(false);
            if hidden {
                continue;
            }
            if p.is_dir() {
                stack.push(p);
            } else if crate::is_audio_file(&p) {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

/// Scan folders, reusing cached entries whose file hasn't changed.
/// Tag reads are spread over all cores.
pub fn scan(roots: &[PathBuf], cache: &[Entry]) -> Vec<Entry> {
    let files: Vec<PathBuf> = roots.iter().flat_map(|r| list_audio_files(r)).collect();
    let cached: std::collections::HashMap<&Path, &Entry> = cache.iter().map(|e| (e.path.as_path(), e)).collect();
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(16);
    let chunk = files.len().div_ceil(threads).max(1);
    let mut out: Vec<Entry> = std::thread::scope(|s| {
        let handles: Vec<_> = files
            .chunks(chunk)
            .map(|part| {
                let cached = &cached;
                s.spawn(move || {
                    part.iter()
                        .map(|p| match cached.get(p.as_path()) {
                            Some(e) if e.modified == mtime(p) => (*e).clone(),
                            Some(e) => {
                                // Re-read changed tags but keep the date it was added.
                                let mut n = Entry::read(p);
                                n.added = e.added;
                                n
                            }
                            _ => Entry::read(p),
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
    });
    out.sort_by(|a, b| a.artist.to_lowercase().cmp(&b.artist.to_lowercase()).then(a.title.cmp(&b.title)));
    out
}
