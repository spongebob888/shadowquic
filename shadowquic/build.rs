use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("android")
        || env::var_os("CARGO_FEATURE_JEMALLOC").is_none()
    {
        return;
    }

    // jemalloc-sys 0.6 requests -lgcc on Android. NDK r23+ replaces that
    // unwinder with libunwind; Clang supplies compiler builtins itself.
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo sets OUT_DIR"));
    fs::write(out.join("libgcc.a"), "INPUT(-lunwind)\n")
        .expect("write Android unwinder compatibility linker script");
    println!("cargo:rustc-link-search=native={}", out.display());
}
