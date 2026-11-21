fn main() {
    println!("cargo::rerun-if-changed=src/wrapper.cpp");
    println!("cargo::rerun-if-changed=vendor");
    let mut b = cc::Build::new();
    b.cpp(true)
        .std("c++14")
        .file("src/wrapper.cpp")
        .include("vendor")
        // Realtime DSP: optimised even in debug builds (tests would crawl otherwise).
        .opt_level(3)
        .warnings(false);
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        // Apple's vDSP FFT.
        b.define("SIGNALSMITH_USE_ACCELERATE", None);
        println!("cargo::rustc-link-lib=framework=Accelerate");
    }
    b.compile("signalsmith-stretch");
}
