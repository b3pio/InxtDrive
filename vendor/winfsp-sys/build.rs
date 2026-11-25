use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let project_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if target_os != "windows" || target_arch != "x86_64" {
        panic!("vendored winfsp-sys only ships x86_64 Windows bindings");
    }

    println!(
        "cargo:rustc-link-search={}",
        project_dir.join("winfsp/lib").to_string_lossy()
    );
    println!("cargo:rustc-link-lib=dylib=delayimp");
    println!("cargo:rustc-link-lib=dylib=winfsp-x64");
    println!("cargo:rustc-link-arg=/DELAYLOAD:winfsp-x64.dll");
    println!("cargo:rerun-if-changed=src/bindings.rs");

    fs::copy(project_dir.join("src/bindings.rs"), out_dir.join("bindings.rs"))
        .expect("copy pregenerated bindings");
}
