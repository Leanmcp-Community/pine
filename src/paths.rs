//! Filesystem layout for `~/.pine`.
//!
//! Logs are write-only and append-only. Nothing in this module ever deletes,
//! truncates, or rotates anything on disk. Sessions accumulate forever.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

/// Root directory: `$PINE_HOME` or `~/.pine`.
pub fn root() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("PINE_HOME") {
        return Ok(PathBuf::from(p));
    }
    let home = dirs::home_dir().ok_or_else(|| anyhow!("could not resolve home directory"))?;
    let root = home.join(".pine");
    migrate_legacy_root(&home, &root);
    Ok(root)
}

/// One-time move of data from the tool's previous name, so existing captures,
/// the root CA and cloud config carry over. A rename, never a delete; does
/// nothing once `~/.pine` exists. Safe to remove after users have upgraded.
fn migrate_legacy_root(home: &Path, root: &Path) {
    let legacy = home.join(concat!(".claude", "tap"));
    if !root.exists() && legacy.is_dir() && std::fs::rename(&legacy, root).is_ok() {
        eprintln!("pine: moved {} to {}", legacy.display(), root.display());
    }
}

pub fn ca_dir() -> Result<PathBuf> {
    Ok(root()?.join("ca"))
}

pub fn ca_cert_path() -> Result<PathBuf> {
    Ok(ca_dir()?.join("root.crt"))
}

pub fn ca_key_path() -> Result<PathBuf> {
    Ok(ca_dir()?.join("root.key"))
}

pub fn sessions_dir() -> Result<PathBuf> {
    Ok(root()?.join("sessions"))
}

pub fn session_dir(session_id: &str) -> Result<PathBuf> {
    Ok(sessions_dir()?.join(session_id))
}

/// Create a directory tree, idempotently. Never deletes anything.
pub fn ensure_dir(p: &Path) -> Result<()> {
    std::fs::create_dir_all(p).with_context(|| format!("creating {}", p.display()))
}
