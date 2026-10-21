//! `pine update` — downloads the latest GitHub release binary for the
//! current platform and replaces the running executable in place.
//!
//! Version identity is the git commit hash embedded by `build.rs` (see
//! `PINE_VERSION` in main.rs), not a hand-bumped semver number: releases
//! are cut continuously from `main`, so "up to date" means "same commit",
//! checked by comparing `--version` output byte-for-byte.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

use crate::PINE_VERSION;

const REPO: &str = "Leanmcp-Community/pine";

fn target_triple() -> Result<&'static str> {
    Ok(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("macos", "aarch64") => "aarch64-apple-darwin",
        (os, arch) => bail!(
            "no prebuilt pine binary for {os}/{arch}; reinstall from source with `cargo install --path .`"
        ),
    })
}

/// Refuse to self-replace binaries owned by a package manager: the manager's
/// own metadata would go stale and the next upgrade/uninstall would misbehave.
fn reject_managed_install(exe: &std::path::Path) -> Result<()> {
    let path = exe.to_string_lossy();
    if path.contains("/Cellar/") || path.contains("/homebrew/") {
        bail!("this binary is managed by Homebrew; run `brew upgrade pine` instead");
    }
    Ok(())
}

pub async fn run() -> Result<()> {
    let target = target_triple()?;
    let exe = std::env::current_exe().context("resolving the running executable's path")?;
    let exe = exe.canonicalize().unwrap_or(exe);
    reject_managed_install(&exe)?;
    let exe_dir = exe
        .parent()
        .context("running executable has no parent directory")?;

    let asset = format!("pine-{target}.tar.gz");
    let base = format!("https://github.com/{REPO}/releases/latest/download");
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()?;

    println!(">>> Checking latest release for {target}");
    let archive = client
        .get(format!("{base}/{asset}"))
        .send()
        .await
        .context("requesting release asset")?
        .error_for_status()
        .context("no release binary is published for this platform yet")?
        .bytes()
        .await
        .context("downloading release asset")?;

    let checksum_text = client
        .get(format!("{base}/{asset}.sha256"))
        .send()
        .await
        .context("requesting checksum")?
        .error_for_status()?
        .text()
        .await?;
    let expected = checksum_text
        .split_whitespace()
        .next()
        .context("malformed checksum file")?;

    let actual = hex::encode(Sha256::digest(&archive));
    if !actual.eq_ignore_ascii_case(expected) {
        bail!("checksum mismatch for {asset}: expected {expected}, got {actual}");
    }

    let tmp_dir = exe_dir.join(format!(".pine-update-{}", ulid::Ulid::new()));
    std::fs::create_dir(&tmp_dir)
        .with_context(|| format!("creating {} (is {} writable?)", tmp_dir.display(), exe_dir.display()))?;
    let cleanup = scopeguard(&tmp_dir);

    let mut tar = Command::new("tar")
        .args(["-xzf", "-", "-C"])
        .arg(&tmp_dir)
        .arg("pine")
        .stdin(Stdio::piped())
        .spawn()
        .context("spawning tar (required on PATH to unpack the release)")?;
    tar.stdin
        .take()
        .context("tar stdin unavailable")?
        .write_all(&archive)
        .context("writing archive to tar")?;
    let status = tar.wait().context("waiting for tar")?;
    if !status.success() {
        bail!("tar exited with {status}");
    }

    let new_bin = tmp_dir.join("pine");
    std::fs::set_permissions(&new_bin, std::fs::Permissions::from_mode(0o755))?;

    let output = Command::new(&new_bin)
        .arg("--version")
        .output()
        .context("running the downloaded binary")?;
    if !output.status.success() {
        bail!("downloaded binary failed to run (--version exited with {})", output.status);
    }
    let new_version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let current_version = format!("pine {PINE_VERSION}");

    if new_version == current_version {
        drop(cleanup);
        println!(">>> Already up to date: {current_version}");
        return Ok(());
    }

    std::fs::rename(&new_bin, &exe)
        .with_context(|| format!("replacing {} (need write access to {})", exe.display(), exe_dir.display()))?;
    drop(cleanup);
    println!(">>> Updated: {current_version} -> {new_version}");
    Ok(())
}

/// Best-effort cleanup of the extraction dir, even on early `?` returns.
fn scopeguard(dir: &std::path::Path) -> impl Drop {
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    Cleanup(dir.to_path_buf())
}
