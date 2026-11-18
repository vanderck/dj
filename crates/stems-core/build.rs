//! macOS + statically linked ONNX Runtime (feature `onnx`): the prebuilt library uses clang's
//! `@available` checks, which call `___isPlatformVersionAtLeast` from clang's runtime library
//! (`libclang_rt.osx.a`). rustc does not link that library, so release builds fail with an
//! undefined symbol. Link it explicitly; `rustc-link-lib` propagates to every final binary
//! (dj, tests, examples).
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let static_onnx = std::env::var_os("CARGO_FEATURE_ONNX").is_some();
    if target_os != "macos" || !static_onnx {
        return;
    }
    match clang_rt_dir() {
        Some(dir) => {
            println!("cargo:rustc-link-search=native={}", dir.display());
            println!("cargo:rustc-link-lib=static=clang_rt.osx");
        }
        None => println!(
            "cargo:warning=libclang_rt.osx.a not found; linking ONNX Runtime may fail with \
             `___isPlatformVersionAtLeast` undefined (install the Xcode command line tools)"
        ),
    }
}

fn clang_rt_dir() -> Option<PathBuf> {
    let out = Command::new("clang")
        .arg("--print-resource-dir")
        .output()
        .ok()?;
    let dir = PathBuf::from(String::from_utf8(out.stdout).ok()?.trim()).join("lib/darwin");
    dir.join("libclang_rt.osx.a").exists().then_some(dir)
}
