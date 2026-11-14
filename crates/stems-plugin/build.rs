//! `STEMS_EMBED_MODEL=/path/model.onnx cargo xtask bundle ...` compiles the model into the
//! plugin binary, so users get a single self-contained plugin file.
fn main() {
    println!("cargo::rustc-check-cfg=cfg(stems_embedded_model)");
    println!("cargo:rerun-if-env-changed=STEMS_EMBED_MODEL");
    if let Ok(path) = std::env::var("STEMS_EMBED_MODEL") {
        if !path.is_empty() {
            println!("cargo:rerun-if-changed={path}");
            println!("cargo:rustc-cfg=stems_embedded_model");
        }
    }
}
