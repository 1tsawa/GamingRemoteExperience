//! Compile libopus 1.6.1 (float, sans SIMD ni DNN) depuis `vendor/opus` avec le compilateur C de la
//! toolchain Rust (cl.exe MSVC via la crate `cc`) : ni cmake, ni autotools, ni bibliotheque systeme.

use std::path::Path;

fn main() {
    let root = Path::new("vendor/opus");
    let list = std::fs::read_to_string("vendor/sources.txt").expect("vendor/sources.txt");
    let mut b = cc::Build::new();
    for f in list.lines().map(str::trim).filter(|l| !l.is_empty()) {
        b.file(root.join(f));
        println!("cargo:rerun-if-changed=vendor/opus/{f}");
    }
    let version = std::fs::read_to_string(root.join("package_version"))
        .ok()
        .and_then(|s| s.split('"').nth(1).map(str::to_owned))
        .unwrap_or_else(|| "1.6.1".into());
    b.include(root)
        .include(root.join("include"))
        .include(root.join("celt"))
        .include(root.join("silk"))
        .include(root.join("silk/float"))
        .include(root.join("src"))
        .define("OPUS_BUILD", None)
        .define("USE_ALLOCA", None)
        .define("HAVE_LRINT", None)
        .define("HAVE_LRINTF", None)
        .define("PACKAGE_VERSION", Some(format!("\"{version}\"").as_str()))
        .opt_level(2)
        .warnings(false)
        .compile("opus");
    println!("cargo:rerun-if-changed=build.rs");
}
