//! Build script — embeds the current git commit (and dirty flag) into the
//! binary as `PINE_GIT_HASH`, so `pine --version` can show e.g.
//!     pine 0.2.0 (a1b2c3d4e5f6)
//!     pine 0.2.0 (a1b2c3d4e5f6-dirty)
//!
//! Falls back to `unknown` if `git` is missing or this is built outside a
//! checkout (e.g. from a `cargo install` source tarball).

use std::process::Command;

fn main() {
    // Short SHA — 12 chars is plenty to disambiguate within this repo and
    // keeps `--version` from looking ridiculous.
    let hash = run_git(&["rev-parse", "--short=12", "HEAD"]).unwrap_or_else(|| "unknown".into());

    // Dirty flag: any tracked-or-untracked change in the working tree.
    let dirty = match Command::new("git").args(["status", "--porcelain"]).output() {
        Ok(out) if out.status.success() => !out.stdout.is_empty(),
        _ => false,
    };

    let suffix = if dirty { "-dirty" } else { "" };
    println!("cargo:rustc-env=PINE_GIT_HASH={hash}{suffix}");

    // Load POSTHOG_API_KEY from .env if present, otherwise set the default key
    let mut posthog_key = "phc_EoMHKFbx6j2wUFsf8ywqgHntY4vEXC3ZzLFoPJVjRRT".to_string();
    if let Ok(env_content) = std::fs::read_to_string(".env") {
        for line in env_content.lines() {
            if let Some(rest) = line.strip_prefix("POSTHOG_API_KEY=") {
                posthog_key = rest.trim().to_string();
                break;
            }
        }
    }
    println!("cargo:rustc-env=POSTHOG_API_KEY={posthog_key}");

    // Re-run when these change so the embedded hash stays in sync. cargo's
    // default rerun rules already cover source files; we just need the
    // git pointers.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs");
    println!("cargo:rerun-if-changed=build.rs");
}

fn run_git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let s = s.trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}
