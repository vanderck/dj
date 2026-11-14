//! Effect catalog (native + VST3) and live effect handles.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, Result};
use collective_engine::fx::native::{NativeFx, ParamSpec};
use collective_engine::fx::Effect;
use collective_vst3::{PluginInfo, Vst3Handle};
use serde::{Deserialize, Serialize};

use crate::settings::{config_dir, load_json, save_json};

/// Serializable form of a VST3 class (for the scan cache).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PluginRecord {
    pub bundle: PathBuf,
    pub cid: String,
    pub name: String,
    pub vendor: String,
    pub version: String,
    pub sub_categories: String,
}

impl PluginRecord {
    pub fn from_info(i: &PluginInfo) -> Self {
        Self {
            bundle: i.bundle.clone(),
            cid: i.id(),
            name: i.name.clone(),
            vendor: i.vendor.clone(),
            version: i.version.clone(),
            sub_categories: i.sub_categories.clone(),
        }
    }

    pub fn to_info(&self) -> Option<PluginInfo> {
        Some(PluginInfo {
            bundle: self.bundle.clone(),
            cid: PluginInfo::cid_from_id(&self.cid)?,
            name: self.name.clone(),
            vendor: self.vendor.clone(),
            version: self.version.clone(),
            sub_categories: self.sub_categories.clone(),
        })
    }

    pub fn category(&self) -> String {
        self.sub_categories.split('|').find(|c| *c != "Fx").unwrap_or("Fx").to_string()
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ScanCache {
    /// bundle path → (mtime, classes or error)
    pub bundles: Vec<(PathBuf, u64, Result<Vec<PluginRecord>, String>)>,
}

impl ScanCache {
    pub fn path() -> PathBuf {
        config_dir().join("vst3-cache.json")
    }
}

fn mtime(p: &Path) -> u64 {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Child-process entry point: `collective --vst3-scan <bundle>` prints JSON.
pub fn scan_child_main(bundle: &Path) -> i32 {
    match collective_vst3::scan_bundle(bundle) {
        Ok(v) => {
            let recs: Vec<PluginRecord> = v.iter().map(PluginRecord::from_info).collect();
            println!("{}", serde_json::to_string(&recs).unwrap_or_default());
            0
        }
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

/// Scan all bundles, each in its own process so a crashing plugin is just
/// reported as failed. Unchanged bundles come from the cache.
pub fn scan_vst3(extra: &[PathBuf]) -> ScanCache {
    let old: ScanCache = load_json(&ScanCache::path());
    let mut roots = collective_vst3::default_search_paths();
    roots.extend(extra.iter().cloned());
    let exe = std::env::current_exe().ok();
    let mut out = ScanCache::default();
    for b in collective_vst3::find_bundles(&roots) {
        let m = mtime(&b);
        if let Some(c) = old.bundles.iter().find(|(p, t, _)| *p == b && *t == m) {
            out.bundles.push(c.clone());
            continue;
        }
        let res = match &exe {
            Some(exe) => match Command::new(exe).arg("--vst3-scan").arg(&b).output() {
                Ok(o) if o.status.success() => serde_json::from_slice::<Vec<PluginRecord>>(&o.stdout)
                    .map_err(|e| format!("bad scan output: {e}")),
                Ok(o) => Err(format!(
                    "scan failed ({}): {}",
                    o.status,
                    String::from_utf8_lossy(&o.stderr).lines().next().unwrap_or("")
                )),
                Err(e) => Err(e.to_string()),
            },
            None => Err("cannot locate executable".into()),
        };
        out.bundles.push((b, m, res));
    }
    let _ = save_json(&ScanCache::path(), &out);
    out
}

/// A catalog entry shown in the insert browser.
#[derive(Clone, Debug)]
pub struct CatalogEntry {
    pub id: String,
    pub name: String,
    pub vendor: String,
    pub category: String,
    pub native: Option<NativeFx>,
    pub vst3: Option<PluginRecord>,
}

pub fn native_entries() -> Vec<CatalogEntry> {
    NativeFx::ALL
        .iter()
        .map(|f| CatalogEntry {
            id: format!("native:{}", f.id()),
            name: f.name().into(),
            vendor: "Collective".into(),
            category: f.category().into(),
            native: Some(*f),
            vst3: None,
        })
        .collect()
}

pub fn vst3_entries(cache: &ScanCache) -> Vec<CatalogEntry> {
    let mut v: Vec<CatalogEntry> = cache
        .bundles
        .iter()
        .filter_map(|(_, _, r)| r.as_ref().ok())
        .flatten()
        .filter(|r| r.to_info().map(|i| !i.is_instrument()).unwrap_or(false))
        .map(|r| CatalogEntry {
            id: r.cid.clone(),
            name: r.name.clone(),
            vendor: if r.vendor.is_empty() { "Unknown".into() } else { r.vendor.clone() },
            category: r.category(),
            native: None,
            vst3: Some(r.clone()),
        })
        .collect();
    v.sort_by(|a, b| a.vendor.cmp(&b.vendor).then(a.name.cmp(&b.name)));
    v
}

/// UI-side handle of a running insert.
#[derive(Clone)]
pub enum FxLive {
    Native(NativeFx),
    Vst3(Vst3Handle),
}

impl FxLive {
    pub fn param_count(&self) -> usize {
        match self {
            FxLive::Native(n) => n.params().len(),
            FxLive::Vst3(h) => h.params.len(),
        }
    }

    pub fn param_name(&self, i: usize) -> String {
        match self {
            FxLive::Native(n) => n.params().get(i).map(|p| p.name.to_string()).unwrap_or_default(),
            FxLive::Vst3(h) => h.params.get(i).map(|p| p.title.clone()).unwrap_or_default(),
        }
    }

    pub fn param_steps(&self, i: usize) -> Option<usize> {
        match self {
            FxLive::Native(n) => n.params().get(i).and_then(ParamSpec::steps),
            FxLive::Vst3(h) => h.params.get(i).and_then(|p| (p.steps > 0).then_some(p.steps as usize + 1)),
        }
    }

    pub fn display(&self, i: usize, v: f32) -> String {
        match self {
            FxLive::Native(n) => n.params().get(i).map(|p| p.format(v)).unwrap_or_default(),
            FxLive::Vst3(h) => h.display(i, v as f64),
        }
    }

    pub fn defaults(&self) -> Vec<f32> {
        match self {
            FxLive::Native(n) => n.params().iter().map(|p| p.default).collect(),
            FxLive::Vst3(h) => (0..h.params.len()).map(|i| h.get_param(i) as f32).collect(),
        }
    }

    pub fn is_vst3(&self) -> bool {
        matches!(self, FxLive::Vst3(_))
    }
}

/// Instantiate an effect by catalog id.
pub fn instantiate(
    id: &str,
    vst3: &[CatalogEntry],
    sample_rate: f32,
    max_block: usize,
) -> Result<(FxLive, Box<dyn Effect>, String, String)> {
    if let Some(nid) = id.strip_prefix("native:") {
        let n = NativeFx::from_id(nid).ok_or_else(|| anyhow!("unknown native effect {nid}"))?;
        return Ok((FxLive::Native(n), n.create(sample_rate), n.name().into(), "Collective".into()));
    }
    let rec = vst3
        .iter()
        .find_map(|e| e.vst3.as_ref().filter(|r| r.cid == id))
        .ok_or_else(|| anyhow!("plugin {id} is not installed"))?;
    let info = rec.to_info().ok_or_else(|| anyhow!("bad plugin id"))?;
    let (h, fx) = collective_vst3::instantiate(&info, sample_rate as f64, max_block).map_err(|e| anyhow!("{e}"))?;
    Ok((FxLive::Vst3(h), Box::new(fx), rec.name.clone(), rec.vendor.clone()))
}

// ------------------------------------------------------------ base64 for plugin state

pub fn b64(data: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(data.len() * 4 / 3 + 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= c.len() {
                s.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                s.push('=');
            }
        }
    }
    s
}

pub fn unb64(s: &str) -> Vec<u8> {
    collective_library::tags::base64_decode(s.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn b64_roundtrip() {
        for n in 0..20 {
            let d: Vec<u8> = (0..n).map(|i| (i * 37 + 11) as u8).collect();
            assert_eq!(unb64(&b64(&d)), d);
        }
    }

    #[test]
    fn natives_instantiate() {
        for e in native_entries() {
            let (live, _fx, name, _) = instantiate(&e.id, &[], 48000.0, 512).unwrap();
            assert_eq!(name, e.name);
            assert_eq!(live.defaults().len(), live.param_count());
        }
    }
}
