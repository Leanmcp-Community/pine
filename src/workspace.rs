//! Beta: workspaces. `pn beta add .` mirrors a repo folder to Pine so
//! teammates can browse it and watch a live feed of what the agent changes.
//!
//! What's sent: every file git would track or show as new (so `.gitignore`
//! decides; `.git/` never goes), up to 1 MB, text only, skipping folders that
//! hold more than 100 files directly (generated stuff). The background job
//! (`pn watch`) checks every couple of seconds and uploads what changed, using
//! size + modified time so unchanged files are never re-read.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::team::{api_base, git, github_repo, load_auth, Auth};

const MAX_FILE: u64 = 1024 * 1024;
const MAX_DIR_FILES: usize = 100;
const BATCH_BYTES: usize = 3 * 1024 * 1024; // under Vercel's 4.5 MB body limit after base64
const BATCH_FILES: usize = 200;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    pub root: String,
    pub repo: String,
    pub folder: String,
}

/// What we last sent for one file.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
struct Seen {
    size: u64,
    mtime: u64,
    sha: String,
}

fn list_path() -> Result<PathBuf> {
    Ok(crate::paths::root()?.join("workspaces.json"))
}
fn cache_path(id: &str) -> Result<PathBuf> {
    Ok(crate::paths::root()?.join("workspaces").join(format!("{id}.json")))
}

pub fn entries() -> Vec<Entry> {
    list_path().ok().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}
fn save_entries(e: &[Entry]) -> Result<()> {
    let p = list_path()?;
    std::fs::create_dir_all(p.parent().unwrap())?;
    std::fs::write(p, serde_json::to_vec_pretty(e)?)?;
    Ok(())
}
fn load_cache(id: &str) -> HashMap<String, Seen> {
    cache_path(id).ok().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}
fn save_cache(id: &str, c: &HashMap<String, Seen>) -> Result<()> {
    let p = cache_path(id)?;
    std::fs::create_dir_all(p.parent().unwrap())?;
    std::fs::write(p, serde_json::to_vec(c)?)?;
    Ok(())
}

fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder().timeout(Duration::from_secs(60)).user_agent(format!("pine/{}", crate::PINE_VERSION)).build()?)
}

// ── scanning ────────────────────────────────────────────────────────────────

/// Never sent, whatever .gitignore says: secrets and keys.
fn secret(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    name.starts_with(".env")
        || name.ends_with(".env")
        || name.starts_with("id_rsa")
        || name.starts_with("id_ed25519")
        || [".pem", ".key", ".p12", ".pfx", ".keystore", ".jks"].iter().any(|x| name.ends_with(x))
        || matches!(name.as_str(), ".npmrc" | ".pypirc" | ".netrc" | "credentials" | "credentials.json" | "secrets.json" | "service-account.json")
        || path.split('/').any(|seg| seg == ".git" || seg == ".ssh" || seg == ".aws")
}

/// Files to mirror (relative paths), and the folders skipped for holding too many files.
fn listing(root: &str) -> Result<(Vec<String>, Vec<String>)> {
    let out = std::process::Command::new("git")
        .args(["-C", root, "ls-files", "--cached", "--others", "--exclude-standard", "-z"])
        .output()
        .context("running git")?;
    if !out.status.success() {
        bail!("{root} isn't a git repo");
    }
    let all: Vec<String> = out
        .stdout
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).to_string())
        .filter(|p| !secret(p))
        .collect();
    let mut per_dir: HashMap<&str, usize> = HashMap::new();
    for p in &all {
        *per_dir.entry(p.rsplit_once('/').map(|(d, _)| d).unwrap_or("")).or_default() += 1;
    }
    let big: Vec<String> = per_dir.iter().filter(|(d, n)| **n > MAX_DIR_FILES && !d.is_empty()).map(|(d, _)| d.to_string()).collect();
    let files = all.iter().filter(|p| !big.iter().any(|d| p.starts_with(&format!("{d}/")) && !p[d.len() + 1..].contains('/'))).cloned().collect();
    let mut big = big;
    big.sort();
    Ok((files, big))
}

fn stat(path: &Path) -> Option<(u64, u64)> {
    let m = std::fs::metadata(path).ok()?;
    if !m.is_file() {
        return None;
    }
    let mtime = m.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_millis() as u64;
    Some((m.len(), mtime))
}

/// Contents to send, or None for binary files.
fn read_text(path: &Path) -> Option<Vec<u8>> {
    let bytes = std::fs::read(path).ok()?;
    (!bytes[..bytes.len().min(8000)].contains(&0)).then_some(bytes)
}

enum Change {
    Put { path: String, bytes: Vec<u8>, seen: Seen },
    Delete { path: String },
}

/// What changed since we last looked (or since `server` says, on the first run).
fn changes(e: &Entry, cache: &HashMap<String, Seen>, server: Option<&HashMap<String, String>>) -> Result<(Vec<Change>, Vec<String>)> {
    let (files, skipped) = listing(&e.root)?;
    let mut out = vec![];
    let mut present = std::collections::HashSet::new();
    for rel in &files {
        let abs = Path::new(&e.root).join(rel);
        let Some((size, mtime)) = stat(&abs) else { continue };
        if size > MAX_FILE {
            continue;
        }
        present.insert(rel.clone());
        if let Some(s) = cache.get(rel) {
            if s.size == size && s.mtime == mtime {
                continue; // untouched: don't even read it
            }
        }
        let Some(bytes) = read_text(&abs) else { continue };
        let sha = hex::encode(Sha256::digest(&bytes));
        let known = cache.get(rel).map(|s| s.sha.as_str()).or_else(|| server.and_then(|m| m.get(rel).map(String::as_str)));
        let seen = Seen { size, mtime, sha: sha.clone() };
        if known == Some(sha.as_str()) {
            out.push(Change::Put { path: rel.clone(), bytes: vec![], seen }); // same contents: just remember it
        } else {
            out.push(Change::Put { path: rel.clone(), bytes, seen });
        }
    }
    let gone = cache.keys().chain(server.map(|m| m.keys()).into_iter().flatten()).filter(|p| !present.contains(*p)).cloned().collect::<std::collections::BTreeSet<_>>();
    out.extend(gone.into_iter().map(|path| Change::Delete { path }));
    Ok((out, skipped))
}

/// Send changes in batches; update the cache as each batch lands. Returns files uploaded.
async fn push(c: &reqwest::Client, auth: &Auth, e: &Entry, cache: &mut HashMap<String, Seen>, list: Vec<Change>, mut progress: impl FnMut(usize)) -> Result<usize> {
    let branch = git(&e.root, &["rev-parse", "--abbrev-ref", "HEAD"]);
    let mut sent = 0;
    let mut batch: Vec<Value> = vec![];
    let mut pending: Vec<(String, Option<Seen>)> = vec![];
    let mut size = 0;
    let url = format!("{}/api/v1/workspaces/{}", api_base(Some(auth)), e.id);

    let mut items = list.into_iter().peekable();
    while items.peek().is_some() || !batch.is_empty() {
        if let Some(ch) = items.next() {
            match ch {
                Change::Put { path, bytes, seen } if bytes.is_empty() => {
                    cache.insert(path, seen);
                }
                Change::Put { path, bytes, seen } => {
                    size += bytes.len() * 4 / 3;
                    batch.push(json!({ "path": path, "content": base64::engine::general_purpose::STANDARD.encode(&bytes) }));
                    pending.push((path, Some(seen)));
                }
                Change::Delete { path } => {
                    batch.push(json!({ "path": path, "deleted": true }));
                    pending.push((path, None));
                }
            }
            if size < BATCH_BYTES && batch.len() < BATCH_FILES && items.peek().is_some() {
                continue;
            }
        }
        if batch.is_empty() {
            continue;
        }
        let res = c.post(&url).bearer_auth(&auth.token).json(&json!({ "branch": branch, "changes": batch })).send().await.context("can't reach Pine (offline?)")?;
        match res.status().as_u16() {
            200 => {}
            401 => bail!("This device was signed out of Pine. Run `pn login`."),
            404 => bail!("Pine doesn't know this workspace any more. Run `pn beta add` in {} again.", e.root),
            s => bail!("upload failed (HTTP {s})"),
        }
        for (path, seen) in pending.drain(..) {
            match seen {
                Some(s) => {
                    cache.insert(path, s);
                    sent += 1;
                }
                None => {
                    cache.remove(&path);
                }
            }
        }
        progress(sent);
        batch.clear();
        size = 0;
    }
    Ok(sent)
}

// ── commands ────────────────────────────────────────────────────────────────

/// `pn beta add [folder]`: start mirroring a repo folder (first upload now, then the background job keeps it live).
pub async fn add(folder: Option<String>) -> Result<()> {
    let auth = load_auth().context("Log in first: pn login")?;
    let start = folder.unwrap_or_else(|| ".".into());
    let root = git(&start, &["rev-parse", "--show-toplevel"]).with_context(|| format!("{start} isn't inside a git repo"))?;
    let repo = git(&root, &["remote", "get-url", "origin"]).and_then(|r| github_repo(&r)).context("This repo has no GitHub remote (origin)")?;
    let name = Path::new(&root).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| repo.clone());
    let branch = git(&root, &["rev-parse", "--abbrev-ref", "HEAD"]);

    let c = client()?;
    let res = c
        .post(format!("{}/api/v1/workspaces", api_base(Some(&auth))))
        .bearer_auth(&auth.token)
        .json(&json!({ "repo": repo, "folder": name, "branch": branch }))
        .send()
        .await
        .context("can't reach Pine (offline?)")?;
    let status = res.status();
    let body: Value = res.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        bail!("{}", body["error"].as_str().unwrap_or("Pine said no"));
    }
    let id = body["id"].as_str().context("Pine didn't return a workspace id")?.to_string();
    let entry = Entry { id: id.clone(), root: root.clone(), repo: repo.clone(), folder: name };
    let mut list = entries();
    list.retain(|x| x.root != root);
    list.push(entry.clone());
    save_entries(&list)?;

    // First upload: only what Pine doesn't already have.
    let server: HashMap<String, String> = match c.get(format!("{}/api/v1/workspaces/{id}", api_base(Some(&auth)))).bearer_auth(&auth.token).send().await {
        Ok(r) if r.status().is_success() => r.json::<Value>().await.ok().and_then(|v| serde_json::from_value(v["files"].clone()).ok()).unwrap_or_default(),
        _ => HashMap::new(),
    };
    let mut cache = HashMap::new();
    let (todo, skipped) = changes(&entry, &cache, Some(&server))?;
    let uploads = todo.iter().filter(|c| matches!(c, Change::Put { bytes, .. } if !bytes.is_empty())).count();
    println!("Workspace {repo} ({root})");
    for d in &skipped {
        println!("  skipping {d}/ (more than {MAX_DIR_FILES} files)");
    }
    println!("  uploading {uploads} file{}…", if uploads == 1 { "" } else { "s" });
    let sent = push(&c, &auth, &entry, &mut cache, todo, |n| {
        if uploads > 50 {
            print!("\r  {n}/{uploads}");
            use std::io::Write;
            let _ = std::io::stdout().flush();
        }
    })
    .await?;
    save_cache(&id, &cache)?;
    if uploads > 50 {
        println!();
    }
    println!("✓ {sent} uploaded. Browse it: {}/workspaces/{id}", api_base(Some(&auth)));
    if crate::watch::running() {
        println!("  The background job keeps it live from here (changes show up within seconds).");
    } else {
        println!("  Turn on the background job to keep it live: pn watch --install");
    }
    Ok(())
}

/// `pn beta remove [folder]`: stop mirroring (what's on Pine stays).
pub fn remove(folder: Option<String>) -> Result<()> {
    let start = folder.unwrap_or_else(|| ".".into());
    let root = git(&start, &["rev-parse", "--show-toplevel"]).unwrap_or(start);
    let mut list = entries();
    let before = list.len();
    list.retain(|e| e.root != root);
    if list.len() == before {
        println!("{root} isn't being synced.");
        return Ok(());
    }
    save_entries(&list)?;
    println!("Stopped syncing {root}. What's already on Pine stays.");
    Ok(())
}

/// `pn beta status`
pub fn status() -> Result<()> {
    let list = entries();
    if list.is_empty() {
        println!("No workspaces. In a repo folder: pn beta add .");
        return Ok(());
    }
    let base = api_base(load_auth().as_ref());
    for e in list {
        let n = load_cache(&e.id).len();
        println!("{}  {}  ({n} files)  {base}/workspaces/{}", e.repo, e.root, e.id);
    }
    println!("Background job: {}", if crate::watch::running() { "running" } else { "off (pn watch --install)" });
    Ok(())
}

/// One pass for the background job: upload whatever changed in every workspace. Returns files sent.
pub async fn tick(auth: &Auth) -> Result<usize> {
    let list = entries();
    if list.is_empty() {
        return Ok(0);
    }
    let c = client()?;
    let mut total = 0;
    for e in list {
        if !Path::new(&e.root).is_dir() {
            continue;
        }
        let mut cache = load_cache(&e.id);
        let (todo, _) = changes(&e, &cache, None)?;
        if todo.is_empty() {
            continue;
        }
        let before = cache.clone();
        let sent = push(&c, auth, &e, &mut cache, todo, |_| {}).await?;
        if cache != before {
            save_cache(&e.id, &cache)?;
        }
        if sent > 0 {
            crate::team::say(&format!("↑ {} · {sent} file{} changed", e.repo, if sent == 1 { "" } else { "s" }));
        }
        total += sent;
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(dir: &Path, args: &[&str]) {
        assert!(std::process::Command::new("git").arg("-C").arg(dir).args(args).output().unwrap().status.success());
    }

    #[test]
    fn sends_new_and_changed_text_files_only() {
        let dir = std::env::temp_dir().join(format!("pine-ws-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        sh(&dir, &["init", "-q"]);
        std::fs::write(dir.join(".gitignore"), "secret.env\n").unwrap();
        std::fs::write(dir.join("src/a.ts"), "export const a = 1\n").unwrap();
        std::fs::write(dir.join("secret.env"), "KEY=1\n").unwrap();
        std::fs::write(dir.join(".env.local"), "KEY=1\n").unwrap();
        std::fs::write(dir.join("logo.bin"), [0u8, 1, 2]).unwrap();
        let e = Entry { id: "t".into(), root: dir.to_string_lossy().into(), repo: "o/r".into(), folder: "r".into() };

        let mut cache = HashMap::new();
        let (first, _) = changes(&e, &cache, None).unwrap();
        let sent: Vec<&str> = first.iter().filter_map(|c| if let Change::Put { path, bytes, .. } = c { (!bytes.is_empty()).then_some(path.as_str()) } else { None }).collect();
        assert!(sent.contains(&"src/a.ts") && sent.contains(&".gitignore"));
        assert!(!sent.contains(&"secret.env"), "gitignored");
        assert!(!sent.contains(&"logo.bin"), "binary");
        for c in first {
            if let Change::Put { path, seen, .. } = c {
                cache.insert(path, seen);
            }
        }

        assert!(changes(&e, &cache, None).unwrap().0.is_empty(), "nothing changed");
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(dir.join("src/a.ts"), "export const a = 2\n").unwrap();
        std::fs::remove_file(dir.join(".gitignore")).unwrap();
        let (next, _) = changes(&e, &cache, None).unwrap();
        assert!(next.iter().any(|c| matches!(c, Change::Put { path, bytes, .. } if path == "src/a.ts" && !bytes.is_empty())));
        assert!(next.iter().any(|c| matches!(c, Change::Delete { path } if path == ".gitignore")));
        assert!(!next.iter().any(|c| matches!(c, Change::Put { path, .. } if path == "secret.env")), "secrets never go, even un-ignored");
        let _ = std::fs::remove_dir_all(dir);
    }
}
