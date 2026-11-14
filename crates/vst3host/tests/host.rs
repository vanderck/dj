//! End-to-end test against a real VST3 binary (the workspace's test plugin).

use std::path::PathBuf;
use std::process::Command;

use collective_engine::fx::{Effect, FxCtx};
use collective_vst3::{find_bundles, instantiate, scan_bundle, PluginInfo};

fn build_fixture() -> PathBuf {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let st = Command::new(cargo)
        .args(["build", "-q", "-p", "collective-vst3-testplugin"])
        .current_dir(&root)
        .status()
        .expect("cargo build");
    assert!(st.success());
    let target = std::env::var("CARGO_TARGET_DIR").map(PathBuf::from).unwrap_or_else(|_| root.join("target"));
    let (lib, sub) = if cfg!(target_os = "windows") {
        ("collective_vst3_testplugin.dll", "x86_64-win/TestGain.vst3")
    } else if cfg!(target_os = "macos") {
        ("libcollective_vst3_testplugin.dylib", "MacOS/TestGain")
    } else {
        ("libcollective_vst3_testplugin.so", "")
    };
    let src = target.join("debug").join(lib);
    let dir = std::env::temp_dir().join(format!("collective-vst3-test-{}", std::process::id()));
    let bundle = dir.join("TestGain.vst3");
    let bin = if sub.is_empty() {
        bundle.join("Contents").join(format!("{}-linux", std::env::consts::ARCH)).join("TestGain.so")
    } else {
        bundle.join("Contents").join(sub)
    };
    std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
    std::fs::copy(&src, &bin).unwrap();
    bundle
}

#[test]
fn scan_instantiate_process_and_restore_state() {
    let bundle = build_fixture();
    let found = find_bundles(&[bundle.parent().unwrap().to_path_buf()]);
    assert_eq!(found, vec![bundle.clone()]);

    let classes = scan_bundle(&bundle).expect("scan");
    assert_eq!(classes.len(), 1);
    let info: PluginInfo = classes[0].clone();
    assert!(info.name.starts_with("Gain"));
    assert_eq!(info.vendor, "Collective Test");
    assert_eq!(PluginInfo::cid_from_id(&info.id()), Some(info.cid));

    let (handle, mut fx) = instantiate(&info, 48000.0, 256).expect("instantiate");
    assert_eq!(handle.params.len(), 1);
    assert_eq!(handle.params[0].title, "Gain");
    assert_eq!(handle.latency(), 0);

    let mut ctx = FxCtx::new(48000.0);
    ctx.bpm = 128.0;
    let mut buf = vec![[0.5f32, -0.25f32]; 300]; // larger than max block: host must chunk
    fx.process(&mut buf, &ctx);
    assert!(buf.iter().all(|f| (f[0] - 0.5).abs() < 1e-6 && (f[1] + 0.25).abs() < 1e-6));

    // Parameter change reaches the processor; controller mirrors it.
    fx.set_param(0, 0.5);
    handle.set_param(0, 0.5);
    let mut buf = vec![[0.5f32, 0.5f32]; 128];
    fx.process(&mut buf, &ctx);
    assert!((buf[0][0] - 0.25).abs() < 1e-6, "{:?}", buf[0]);
    assert!((handle.get_param(0) - 0.5).abs() < 1e-9);
    assert_eq!(handle.display(0, 0.5), "0.5");

    // State round-trip into a fresh instance.
    let state = handle.save_state();
    assert_eq!(state.component.len(), 8);
    let (h2, mut fx2) = instantiate(&info, 48000.0, 256).unwrap();
    h2.load_state(&state);
    assert!((h2.get_param(0) - 0.5).abs() < 1e-9, "controller got component state");
    let mut buf = vec![[1.0f32, 1.0f32]; 64];
    fx2.process(&mut buf, &ctx);
    assert!((buf[0][0] - 0.5).abs() < 1e-6, "processor restored: {:?}", buf[0]);
    assert!(!fx2.failed());

    drop(fx);
    drop(handle);
    let _ = std::fs::remove_dir_all(bundle.parent().unwrap());
}
