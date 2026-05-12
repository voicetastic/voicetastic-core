//! Resolve the user-facing version at build time.
//!
//! Resolution order:
//!   1. `VOICETASTIC_VERSION` env var (set by CI/packagers) — highest priority.
//!   2. `git describe --tags --match 'v[0-9]*' --dirty=+dirty` — local dev builds.
//!   3. Fallback string `0.0.0-unknown` — e.g. source tarballs with no git.
//!
//! The resolved value is exposed to the crate via `env!("VOICETASTIC_VERSION")`.

use std::process::Command;

fn main() {
    // Re-run when the git HEAD or tag set changes, or when the override env
    // var changes. `cargo:rerun-if-changed` paths that don't exist are
    // silently ignored, which is the behaviour we want in source tarballs.
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/refs/tags");
    println!("cargo:rerun-if-changed=../../.git/packed-refs");
    println!("cargo:rerun-if-env-changed=VOICETASTIC_VERSION");

    if let Ok(v) = std::env::var("VOICETASTIC_VERSION") {
        let v = v.trim();
        if !v.is_empty() {
            println!("cargo:rustc-env=VOICETASTIC_VERSION={v}");
            return;
        }
    }

    let version = Command::new("git")
        .args([
            "describe",
            "--tags",
            "--match",
            "v[0-9]*",
            "--dirty=+dirty",
            "--always",
        ])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| {
            let s = String::from_utf8(o.stdout).ok()?;
            let s = s.trim();
            if s.is_empty() {
                None
            } else {
                // Strip leading `v` from tag-based descriptions
                // (`v1.2.3`, `v1.2.3-5-gabcdef`, `v1.2.3+dirty`, …).
                Some(s.strip_prefix('v').unwrap_or(s).to_string())
            }
        })
        .unwrap_or_else(|| "0.0.0-unknown".to_string());

    println!("cargo:rustc-env=VOICETASTIC_VERSION={version}");
}
