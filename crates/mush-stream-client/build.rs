//! Auto-copy ffmpeg shared-library DLLs from `$FFMPEG_DIR/bin/` into the
//! cargo target/profile directory so `cargo run` and the produced .exe
//! both find them adjacent to the binary at runtime.
//!
//! Triggered when `FFMPEG_DIR` is set (which it must be for the link step
//! anyway). On non-Windows or when the DLLs aren't present (e.g. a static
//! ffmpeg build), this script silently does nothing.

use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    println!("cargo::rerun-if-env-changed=FFMPEG_DIR");

    let Ok(ffmpeg_dir) = env::var("FFMPEG_DIR") else {
        return;
    };
    let bin_dir = PathBuf::from(&ffmpeg_dir).join("bin");
    if !bin_dir.is_dir() {
        return;
    }
    println!("cargo::rerun-if-changed={}", bin_dir.display());

    // OUT_DIR = target/{profile}/build/{crate}-{hash}/out
    // Climb three ancestors to land at target/{profile}.
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR must be set in build scripts"));
    let Some(profile_dir) = out_dir.ancestors().nth(3) else {
        println!("cargo::warning=OUT_DIR has fewer than 3 ancestors; skipping DLL copy");
        return;
    };
    let deps_dir = profile_dir.join("deps");

    let entries = match fs::read_dir(&bin_dir) {
        Ok(e) => e,
        Err(e) => {
            println!(
                "cargo::warning=failed to read {}: {}",
                bin_dir.display(),
                e
            );
            return;
        }
    };

    for entry in entries.flatten() {
        let src = entry.path();
        if src.extension().and_then(|s| s.to_str()) != Some("dll") {
            continue;
        }
        let Some(name) = src.file_name() else { continue };

        for dst_dir in [profile_dir, &deps_dir] {
            let dst = dst_dir.join(name);
            if is_up_to_date(&src, &dst) {
                continue;
            }
            if let Some(parent) = dst.parent()
                && !parent.exists()
                && let Err(e) = fs::create_dir_all(parent)
            {
                println!("cargo::warning=mkdir {}: {}", parent.display(), e);
                continue;
            }
            if let Err(e) = fs::copy(&src, &dst) {
                println!(
                    "cargo::warning=failed to copy {} -> {}: {}",
                    src.display(),
                    dst.display(),
                    e
                );
            }
        }
    }
}

fn is_up_to_date(src: &std::path::Path, dst: &std::path::Path) -> bool {
    let (Ok(src_meta), Ok(dst_meta)) = (src.metadata(), dst.metadata()) else {
        return false;
    };
    let (Ok(src_mtime), Ok(dst_mtime)) = (src_meta.modified(), dst_meta.modified()) else {
        return false;
    };
    dst_mtime >= src_mtime
}
