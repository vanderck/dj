//! Picks the separator for a given host configuration: the ONNX model when one is available and
//! compatible, otherwise the placeholder heuristic. Runs in `initialize` (allocation allowed).

use std::sync::Arc;

use stems_core::{SeparatorStats, StemEngine};

/// What the UI shows about the active separator.
#[derive(Clone, Default)]
pub(crate) struct SeparatorInfo {
    pub name: String,
    /// Why the model is not in use, if it is not.
    pub note: Option<String>,
    pub stats: Option<Arc<SeparatorStats>>,
}

pub(crate) fn build_engine(sample_rate: f32, offline: bool) -> (StemEngine, SeparatorInfo) {
    let (mut engine, info) = match model_engine(sample_rate) {
        Ok(found) => found,
        Err(note) => (
            StemEngine::new(sample_rate),
            SeparatorInfo {
                name: "placeholder heuristic".into(),
                note: Some(note),
                stats: None,
            },
        ),
    };
    engine.set_offline(offline);
    (engine, info)
}

#[cfg(not(any(feature = "onnx", feature = "onnx-dynamic")))]
fn model_engine(_sample_rate: f32) -> Result<(StemEngine, SeparatorInfo), String> {
    Err("built without ONNX support (enable the `onnx` feature)".into())
}

#[cfg(any(feature = "onnx", feature = "onnx-dynamic"))]
fn model_engine(sample_rate: f32) -> Result<(StemEngine, SeparatorInfo), String> {
    use stems_core::{MaskModel, ThreadedSeparator};

    let model = load_model()?;
    let spec = model.spec();
    let name = model.name().to_string();
    if !spec.matches_sample_rate(sample_rate) {
        return Err(format!(
            "model `{name}` runs at {} Hz but the host runs at {sample_rate} Hz \
             (internal resampling is not implemented yet)",
            spec.sample_rate
        ));
    }
    let pipeline = pipeline_frames();
    let separator = ThreadedSeparator::new(model, pipeline).map_err(|e| e.to_string())?;
    let stats = separator.stats();
    let engine = StemEngine::with_separator(sample_rate, spec.fft_size, Box::new(separator));
    Ok((
        engine,
        SeparatorInfo {
            name,
            note: None,
            stats: Some(stats),
        },
    ))
}

/// Model source, in order: `STEMS_MODEL` (path), then a model embedded at build time.
#[cfg(any(feature = "onnx", feature = "onnx-dynamic"))]
fn load_model() -> Result<stems_core::OnnxModel, String> {
    use stems_core::{OnnxModel, OnnxOptions};

    let options = OnnxOptions::default();
    if let Some(path) = std::env::var_os("STEMS_MODEL").filter(|p| !p.is_empty()) {
        return OnnxModel::from_file(&path, &options).map_err(|e| e.to_string());
    }

    #[cfg(stems_embedded_model)]
    {
        static EMBEDDED: &[u8] = include_bytes!(env!("STEMS_EMBED_MODEL"));
        OnnxModel::from_memory(EMBEDDED, &options).map_err(|e| e.to_string())
    }

    #[cfg(not(stems_embedded_model))]
    Err("no model: set STEMS_MODEL or build with STEMS_EMBED_MODEL".into())
}

/// Worker budget in hops (`STEMS_PIPELINE_FRAMES`, default 2). More = fewer underruns on slow
/// machines, at the cost of latency.
#[cfg(any(feature = "onnx", feature = "onnx-dynamic"))]
fn pipeline_frames() -> usize {
    std::env::var("STEMS_PIPELINE_FRAMES")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&n: &usize| (1..=32).contains(&n))
        .unwrap_or(stems_core::ThreadedSeparator::DEFAULT_PIPELINE_FRAMES)
}

#[cfg(all(test, any(feature = "onnx", feature = "onnx-dynamic")))]
mod tests {
    use super::*;

    // One test so the STEMS_MODEL mutations cannot race with each other.
    #[test]
    fn picks_model_and_falls_back_with_a_reason() {
        let model = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../stems-core/tests/data/test_model.onnx"
        );
        std::env::set_var("STEMS_MODEL", model);

        let (engine, info) = build_engine(44_100.0, true);
        assert!(info.name.contains("test model"), "{}", info.name);
        assert!(info.note.is_none());
        assert!(info.stats.is_some());
        assert_eq!(engine.latency_samples(), 4096 + 4 * 1024);

        let (_, info) = build_engine(48_000.0, false);
        assert_eq!(info.name, "placeholder heuristic");
        assert!(info.note.unwrap().contains("44100"));

        std::env::set_var("STEMS_MODEL", "/nonexistent/model.onnx");
        let (engine, info) = build_engine(44_100.0, false);
        assert!(info.note.unwrap().contains("nonexistent"));
        assert_eq!(engine.latency_samples(), 4096);

        std::env::remove_var("STEMS_MODEL");
    }
}
