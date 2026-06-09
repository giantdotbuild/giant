//! Build script.
//!
//! Captures the target triple so we can mix it into cache keys at runtime
//! Without this, a binary cached on Linux could mis-restore on macOS when
//! the cache directory is shared.

fn main() {
    let triple = std::env::var("TARGET").unwrap_or_else(|_| "unknown".into());
    println!("cargo:rustc-env=GIANT_TARGET_TRIPLE={triple}");
    println!("cargo:rerun-if-changed=build.rs");
}
