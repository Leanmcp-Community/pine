//! Team sync with Pine (app.usepine.dev).
//!
//! Recording is always local (`~/.pine/sessions/<ulid>/`). When logged in,
//! the same files are streamed to Pine so teammates who can read the
//! session's GitHub repo can watch, checkpoint and pull it.
//!
//! - `pine login`: GitHub device flow against the "Pine MD" GitHub App, then
//!   the GitHub token is exchanged for a Pine CLI token (`~/.pine/auth.json`).
//! - Live sync: while a launcher runs, new bytes upload every ~2s. Uploads are
//!   append-only chunks (≤3 MB) keyed by (session, file, offset), so retries
//!   and restarts are safe. Progress lives in `~/.pine/team-sync.json`.
//! - Offline or logged out: nothing changes locally; sync catches up later.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use clap::Subcommand;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const DEFAULT_API: &str = "https://app.usepine.dev";
/// Public client id of the "Pine MD" GitHub App (device flow needs no secret).
const GITHUB_CLIENT_ID: &str = "Iv23liqQWqjQv0ivKzAi";
const MAX_TRANSCRIPT: usize = 3_800_000; // stays under Vercel's 4.5 MB body limit
const LIVE_EVERY: Duration = Duration::from_secs(2);
/// How often the background job (`pn watch`) checks for a new release.
const UPDATE_EVERY: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Subcommand)]
pub enum SyncCmd {
    /// Show what's uploaded and what's still pending.
    Status,
    /// Stop uploading (recording continues locally).
    Pause,
    /// Resume uploading.
    Resume,
}

// ── Auth ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Auth {
    pub api: String,
    pub token: String,
    pub login: String,
    pub name: String,
    pub email: String,
    pub device_id: String,
    /// Sessions that started before this moment (ms since epoch) are not
    /// shared unless you run `pn sync --all`.
    #[serde(default)]
    pub since_ms: u64,
}

fn root() -> Result<PathBuf> {
    crate::paths::root()
}

fn api_base(auth: Option<&Auth>) -> String {
    std::env::var("PINE_API")
        .ok()
        .or_else(|| auth.map(|a| a.api.clone()))
        .unwrap_or_else(|| DEFAULT_API.to_string())
        .trim_end_matches('/')
        .to_string()
}

pub fn load_auth() -> Option<Auth> {
    let bytes = std::fs::read(root().ok()?.join("auth.json")).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp).with_context(|| format!("writing {}", tmp.display()))?;
        std::io::Write::write_all(&mut f, bytes)?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .user_agent(format!("pine/{}", crate::PINE_VERSION))
        .build()?)
}

fn require_auth() -> Result<Auth> {
    load_auth().ok_or_else(|| anyhow!("Not logged in. Run `pn login` first."))
}

async fn api_json(req: reqwest::RequestBuilder) -> Result<Value> {
    let res = req.send().await.context("can't reach Pine (offline?)")?;
    let status = res.status();
    let body: Value = res.json().await.unwrap_or(Value::Null);
    if status.is_success() {
        return Ok(body);
    }
    let msg = body["error"].as_str().unwrap_or("request failed");
    bail!("{msg} (HTTP {})", status.as_u16())
}

fn open_browser(url: &str) {
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    let _ = std::process::Command::new(opener)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// `pine login`: GitHub device flow → Pine token.
pub async fn login() -> Result<()> {
    let c = client()?;
    let code: Value = c
        .post("https://github.com/login/device/code")
        .header("Accept", "application/json")
        .json(&json!({ "client_id": GITHUB_CLIENT_ID }))
        .send()
        .await
        .context("can't reach github.com")?
        .json()
        .await?;
    let device_code = code["device_code"].as_str().ok_or_else(|| anyhow!("GitHub didn't return a device code: {code}"))?;
    let user_code = code["user_code"].as_str().unwrap_or_default();
    let uri = code["verification_uri"].as_str().unwrap_or("https://github.com/login/device");
    let mut interval = code["interval"].as_u64().unwrap_or(5);

    println!("\n  Open {uri} and enter the code:\n\n      {user_code}\n");
    open_browser(uri);
    println!("  Waiting for you to approve Pine MD on GitHub…");

    let tokens = loop {
        tokio::time::sleep(Duration::from_secs(interval)).await;
        let r: Value = c
            .post("https://github.com/login/oauth/access_token")
            .header("Accept", "application/json")
            .json(&json!({
                "client_id": GITHUB_CLIENT_ID,
                "device_code": device_code,
                "grant_type": "urn:ietf:params:oauth:grant-type:device_code",
            }))
            .send()
            .await?
            .json()
            .await?;
        if r["access_token"].is_string() {
            break r;
        }
        match r["error"].as_str() {
            Some("authorization_pending") => {}
            Some("slow_down") => interval += 5,
            Some("expired_token") => bail!("The code expired. Run `pn login` again."),
            Some("access_denied") => bail!("Login was cancelled on GitHub."),
            other => bail!("GitHub login failed: {}", other.unwrap_or("unknown error")),
        }
    };

    let api = api_base(None);
    let res = api_json(c.post(format!("{api}/api/cli/exchange")).json(&json!({
        "github_token": tokens["access_token"],
        "github_refresh_token": tokens["refresh_token"],
        "expires_in": tokens["expires_in"],
        "refresh_token_expires_in": tokens["refresh_token_expires_in"],
        "device": {
            "hostname": crate::hostname().unwrap_or_else(|| "unknown".into()),
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "version": crate::PINE_VERSION,
        },
    })))
    .await?;

    let auth = Auth {
        api,
        token: res["token"].as_str().ok_or_else(|| anyhow!("Pine returned no token"))?.to_string(),
        login: res["user"]["login"].as_str().unwrap_or_default().to_string(),
        name: res["user"]["name"].as_str().unwrap_or_default().to_string(),
        email: res["user"]["email"].as_str().unwrap_or_default().to_string(),
        device_id: res["device_id"].as_str().unwrap_or_default().to_string(),
        since_ms: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0),
    };
    write_private(&root()?.join("auth.json"), &serde_json::to_vec_pretty(&auth)?)?;
    println!("\n✓ Logged in as @{} ({}).", auth.login, auth.email);
    println!("  Sessions now sync live to {}. Teammates who can read the same GitHub repo can see them.", auth.api);
    Ok(())
}

/// `pine logout`: revoke this device's token, keep recording locally.
pub async fn logout() -> Result<()> {
    let Some(auth) = load_auth() else {
        println!("Not logged in.");
        return Ok(());
    };
    let api = api_base(Some(&auth));
    let revoked = client()?
        .post(format!("{api}/api/cli/logout"))
        .bearer_auth(&auth.token)
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false);
    std::fs::remove_file(root()?.join("auth.json"))?;
    println!(
        "✓ Logged out{}. Pine keeps recording locally to ~/.pine/sessions.",
        if revoked { "" } else { " locally (couldn't reach Pine to revoke the token; sign this device out on the Devices page)" }
    );
    Ok(())
}

/// `pine whoami`
pub async fn whoami() -> Result<()> {
    let Some(auth) = load_auth() else {
        println!("Not logged in. Recording locally to {}.", crate::paths::sessions_dir()?.display());
        println!("Run `pn login` to share sessions with your team.");
        return Ok(());
    };
    let api = api_base(Some(&auth));
    let pending = pending_sessions().unwrap_or(0);
    let sync = if sync_paused() {
        "paused (`pn sync resume`)".to_string()
    } else if pending > 0 {
        format!("on, {} waiting to upload", sessions_label(pending))
    } else {
        "on, up to date".to_string()
    };
    match api_json(client()?.get(format!("{api}/api/v1/me")).bearer_auth(&auth.token)).await {
        Ok(me) => {
            println!("@{} · {} · {}", me["user"]["login"].as_str().unwrap_or(&auth.login), me["user"]["name"].as_str().unwrap_or(&auth.name), me["user"]["email"].as_str().unwrap_or(&auth.email));
            println!("Device: {} ({})", me["device"]["hostname"].as_str().unwrap_or("?"), api);
            println!("Sync:   {sync}");
        }
        Err(e) if e.to_string().contains("HTTP 401") => {
            println!("This device was signed out of Pine. Recording continues locally; run `pn login`.");
        }
        Err(_) => {
            println!("@{} · {} · {} (offline, cached)", auth.login, auth.name, auth.email);
            println!("Sync:   {sync}. Recording locally; uploads resume when you're back online.");
        }
    }
    Ok(())
}

// ── Sync ────────────────────────────────────────────────────────────────────

#[derive(Debug, Default, Serialize, Deserialize)]
struct SyncState {
    sessions: BTreeMap<String, SessionSync>,
    /// Set once the native-first rule has re-checked sessions skipped under the old rule.
    #[serde(default)]
    native_first: bool,
    /// Set once every session was re-sent with its agent title (Claude ai-title, Codex thread name).
    #[serde(default)]
    titles_sent: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct SessionSync {
    registered: bool,
    /// Checkpoint offsets for this session: {"transcript.json": message count}.
    files: BTreeMap<String, u64>,
    /// Total raw size when the transcript was last uploaded (change detection).
    #[serde(default)]
    sig: u64,
    /// Deleted on the web (the server answered 410): never upload it again.
    #[serde(default)]
    deleted: bool,
    /// Never uploaded: a Codex sub-agent thread, or a pn run whose
    /// conversation Claude Code / Codex also saved (that file is shared).
    #[serde(default)]
    skip: bool,
}

fn state_path() -> Result<PathBuf> {
    Ok(root()?.join("team-sync.json"))
}

fn load_state() -> SyncState {
    state_path()
        .ok()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save_state(s: &SyncState) -> Result<()> {
    write_private(&state_path()?, &serde_json::to_vec(s)?)
}

fn sync_paused() -> bool {
    root().map(|r| r.join("sync-paused").exists()).unwrap_or(false)
}

fn is_ulid(s: &str) -> bool {
    s.len() == 26 && s.parse::<ulid::Ulid>().is_ok()
}

/// Session dirs recorded on this machine (pulled copies excluded), oldest first.
/// Only sessions started after `since_ms` (the ULID encodes its start time).
fn local_sessions_since(since_ms: u64) -> Result<Vec<(String, PathBuf)>> {
    let dir = crate::paths::sessions_dir()?;
    let mut out = vec![];
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let p = e.path();
            let started = name.parse::<ulid::Ulid>().map(|u| u.timestamp_ms()).unwrap_or(0);
            if is_ulid(&name) && started >= since_ms && p.is_dir() && !p.join(".pulled").exists() {
                out.push((name, p));
            }
        }
    }
    out.sort();
    Ok(out)
}

/// The sessions sync covers: since login, or everything with `--all`.
fn local_sessions(all: bool) -> Result<Vec<(String, PathBuf)>> {
    if all {
        return local_sessions_since(0);
    }
    let since = load_auth().map(|a| a.since_ms).unwrap_or(u64::MAX);
    let shared = load_state();
    Ok(local_sessions_since(0)?
        .into_iter()
        .filter(|(id, _)| id.parse::<ulid::Ulid>().is_ok_and(|u| u.timestamp_ms() >= since) || already_shared(&shared, id))
        .collect())
}

/// Shared before (e.g. with `pn sync --all`): it keeps syncing as it grows,
/// even though it started before `pn login`.
fn already_shared(state: &SyncState, id: &str) -> bool {
    state.sessions.get(id).is_some_and(|s| s.registered && !s.deleted && !s.skip)
}

/// Every uploadable file under a session dir. meta.json is rewritten in place
/// (not append-only), so it travels as structured metadata instead.
fn session_files(dir: &Path) -> Vec<(String, u64)> {
    fn walk(base: &Path, d: &Path, out: &mut Vec<(String, u64)>) {
        let Ok(rd) = std::fs::read_dir(d) else { return };
        for e in rd.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            if p.is_dir() {
                walk(base, &p, out);
            } else if let (Ok(rel), Ok(m)) = (p.strip_prefix(base), e.metadata()) {
                let rel = rel.to_string_lossy().replace('\\', "/");
                if rel != "meta.json" && safe_name(&rel) {
                    out.push((rel, m.len()));
                }
            }
        }
    }
    let mut out = vec![];
    walk(dir, dir, &mut out);
    out.sort();
    out
}

/// Same rule as the server: plain path segments only (so it is URL-safe as-is).
fn safe_name(rel: &str) -> bool {
    !rel.is_empty()
        && rel.len() <= 200
        && rel.split('/').all(|seg| !seg.is_empty() && seg != ".." && seg.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)))
}

/// Sessions whose conversation changed since it was last uploaded.
fn pending_sessions() -> Result<usize> {
    let state = load_state();
    let mut n = 0;
    for (id, dir) in local_sessions(false)? {
        let sig: u64 = session_files(&dir).iter().map(|(_, len)| len).sum();
        if state.sessions.get(&id).is_none_or(|s| !s.deleted && s.sig != sig) {
            n += 1;
        }
    }
    for f in native_sessions(false) {
        if state.sessions.get(&f.id).is_none_or(|s| !s.deleted && !s.skip && s.sig != f.size) {
            n += 1;
        }
    }
    Ok(n)
}

fn sessions_label(n: usize) -> String {
    if n == 1 {
        "1 session".into()
    } else {
        format!("{n} sessions")
    }
}

pub(crate) fn git(cwd: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git").arg("-C").arg(cwd).args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string()).filter(|s| !s.is_empty())
}

/// "owner/name" from a GitHub remote (https or ssh form).
fn github_repo(remote: &str) -> Option<String> {
    let rest = remote
        .strip_prefix("git@github.com:")
        .or_else(|| remote.strip_prefix("ssh://git@github.com/"))
        .or_else(|| remote.split_once("github.com/").map(|(_, r)| r))?;
    let rest = rest.trim_end_matches('/').trim_end_matches(".git");
    let mut parts = rest.splitn(3, '/');
    let (owner, name) = (parts.next()?, parts.next()?);
    (!owner.is_empty() && !name.is_empty()).then(|| format!("{owner}/{name}"))
}

/// Remove a session this device uploaded earlier (a pn copy of a conversation
/// now shared from Claude Code's or Codex's own file). Gone already is fine.
async fn remove_remote(c: &reqwest::Client, auth: &Auth, id: &str) -> Result<()> {
    let res = c
        .delete(format!("{}/api/v1/sessions/{id}", api_base(Some(auth))))
        .bearer_auth(&auth.token)
        .send()
        .await
        .context("can't reach Pine (offline?)")?;
    match res.status().as_u16() {
        200 | 404 | 410 => Ok(()),
        401 => bail!("This device was signed out of Pine. Run `pn login`."),
        s => bail!("removing a duplicate session failed (HTTP {s})"),
    }
}

/// Tell Pine about a session (repo, branch, commit). Returns Pine's note when
/// the repo couldn't be verified and the session stays private.
async fn register(c: &reqwest::Client, auth: &Auth, id: &str, meta: &crate::native::Meta, agent: &str) -> Result<Option<String>> {
    let cwd = meta.cwd.as_str();
    let repo = if cwd.is_empty() { None } else { git(cwd, &["remote", "get-url", "origin"]).and_then(|r| github_repo(&r)) };
    let title = meta.title.clone().or_else(|| repo.clone()).or_else(|| Path::new(cwd).file_name().map(|n| n.to_string_lossy().to_string()));
    let res = api_json(
        c.put(format!("{}/api/v1/sessions/{id}", api_base(Some(auth))))
            .bearer_auth(&auth.token)
            .json(&json!({
                "repo": repo,
                "branch": if cwd.is_empty() { None } else { git(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]) },
                "commit": if cwd.is_empty() { None } else { git(cwd, &["rev-parse", "--short", "HEAD"]) },
                "agent": agent,
                "title": title,
                "started_at": if meta.started_at.is_empty() { now_rfc3339() } else { meta.started_at.clone() },
            })),
    )
    .await?;
    Ok(res["note"].as_str().map(String::from))
}

pub(crate) fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

/// A sync log line. In a terminal it clears the in-place "watching" line
/// first; in a log file (`pn watch`) it gets a UTC timestamp instead.
fn say(msg: &str) {
    use std::io::{IsTerminal, Write};
    if std::io::stdout().is_terminal() {
        println!("\r\x1b[K{msg}");
    } else {
        let fmt = time::macros::format_description!("[year]-[month]-[day] [hour]:[minute]:[second]Z");
        println!("{} {msg}", time::OffsetDateTime::now_utc().format(&fmt).unwrap_or_default());
    }
    let _ = std::io::stdout().flush();
}

/// The background watcher (`pn watch`, run by launchd / systemd): `pn sync`
/// that never exits. Logged out or paused: it waits and checks again.
pub async fn watch_loop() -> Result<()> {
    say(&format!("Pine watch started ({}).", crate::PINE_VERSION));
    let (mut waiting, mut last_err) = (String::new(), String::new());
    let mut next_update = std::time::Instant::now() + Duration::from_secs(60); // first check a minute after start
    loop {
        if std::time::Instant::now() >= next_update {
            next_update = std::time::Instant::now() + UPDATE_EVERY;
            match crate::update::auto().await {
                Ok(Some(v)) => {
                    say(&format!("Updated to {v}. Restarting on the new version."));
                    crate::telemetry::send("cli_auto_updated", json!({ "to": v })).wait();
                    std::process::exit(0); // launchd / systemd start it again
                }
                Ok(None) => {}
                Err(e) => say(&format!("! update check failed: {e:#}")),
            }
        }
        let why = if load_auth().is_none() {
            "not logged in (run `pn login`)"
        } else if sync_paused() {
            "sync is paused (run `pn sync resume`)"
        } else {
            ""
        };
        if why != waiting {
            say(&if why.is_empty() { "Syncing.".to_string() } else { format!("Waiting: {why}.") });
            waiting = why.to_string();
        }
        if why.is_empty() {
            match sync_once(true, false).await {
                Ok(_) => last_err.clear(),
                // Offline etc.: log it once, not every 2 seconds.
                Err(e) if e.to_string() != last_err => {
                    say(&format!("! {e:#}. Retrying."));
                    last_err = e.to_string();
                }
                Err(_) => {}
            }
        }
        tokio::time::sleep(if why.is_empty() { LIVE_EVERY } else { Duration::from_secs(15) }).await;
    }
}

/// Where a session's conversation comes from.
enum Source<'a> {
    /// Recorded by `pn` / `pn codex` in `~/.pine/sessions/<id>`.
    Pine(&'a Path),
    /// Saved by Claude Code or Codex themselves: plain CLI or desktop app.
    Native(&'a crate::native::Native),
}

/// The transcript as uploaded, with the agent's own title (Claude Code's ai-title,
/// Codex's thread name) when it has one: Pine shows that title as is.
fn upload_body(t: &crate::transcript::Transcript, title: Option<&str>) -> Result<Vec<u8>> {
    let mut v = serde_json::to_value(t)?;
    if let Some(title) = title.map(str::trim).filter(|s| !s.is_empty()) {
        v["title"] = Value::from(title);
    }
    Ok(serde_json::to_vec(&v)?)
}

/// Upload one session's conversation if anything changed. Builds a compact
/// transcript locally (src/transcript.rs, src/native.rs) and replaces it on
/// Pine; raw files never leave this machine. Returns bytes uploaded.
async fn sync_session(
    c: &reqwest::Client,
    auth: &Auth,
    state: &mut SyncState,
    natives: &crate::native::NativeIndex,
    id: &str,
    src: Source<'_>,
    verbose: bool,
) -> Result<u64> {
    // Files only ever grow, so their size is a cheap change signal.
    let sig: u64 = match src {
        Source::Pine(dir) => session_files(dir).iter().map(|(_, len)| len).sum(),
        Source::Native(n) => n.size,
    };
    let entry = state.sessions.entry(id.to_string()).or_default();
    // One session per conversation: if Claude Code or Codex saved this pn run's
    // conversation itself, that file is shared instead (sync picks it up as a
    // native session). This copy stays on disk; an earlier upload is removed.
    // Same for a run that only did Claude Code's start-up quota check (tiny,
    // so the check is cheap): nothing to share.
    if let Source::Pine(dir) = src {
        if !entry.skip && !entry.deleted && (natives.has_native_copy(id, dir) || (sig < 256 * 1024 && crate::transcript::quota_only(dir))) {
            if entry.registered {
                remove_remote(c, auth, id).await?;
                if verbose {
                    say(&format!("· {}…: a duplicate or empty pn run, removed from Pine (still on this machine)", &id[..id.len().min(8)]));
                }
            }
            entry.skip = true;
            return Ok(0);
        }
    }
    if entry.deleted || entry.skip || (entry.registered && entry.sig == sig) {
        return Ok(0);
    }
    let (t, meta, agent, origin) = match src {
        Source::Pine(dir) => {
            let m: Value = std::fs::read(dir.join("meta.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null);
            let meta = crate::native::Meta {
                cwd: m["cwd"].as_str().unwrap_or_default().to_string(),
                started_at: m["started_at"].as_str().unwrap_or_default().to_string(),
                title: None,
            };
            let agent = m["target"].as_str().unwrap_or("claude").to_string();
            (crate::transcript::build(dir), meta, agent, "pn")
        }
        Source::Native(n) => {
            let Some((t, meta)) = crate::native::read(n) else {
                if n.agent == "codex" {
                    entry.skip = true; // a sub-agent thread: it's part of its parent
                } else {
                    entry.sig = sig; // a copy made by pn: look again once it grows
                }
                return Ok(0);
            };
            (Some(t), meta, n.agent.to_string(), n.agent)
        }
    };
    let folder = Path::new(&meta.cwd).file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_else(|| "no folder".into());
    let label = format!("{}… {origin} ({folder})", &id[..id.len().min(8)]);
    if verbose {
        say(&format!("→ {label}: new activity, uploading…"));
    }
    if !entry.registered {
        match register(c, auth, id, &meta, &agent).await {
            Ok(Some(note)) if verbose => say(&format!("  {label}: {note}")),
            Ok(_) => {}
            Err(e) if e.to_string().ends_with("(HTTP 410)") => {
                if verbose {
                    say(&format!("· {label}: deleted on the web, skipping it from now on"));
                }
                entry.deleted = true;
                return Ok(0);
            }
            Err(e) => return Err(e),
        }
        entry.registered = true;
    }
    let Some(mut t) = t.filter(|t| !t.messages.is_empty()) else {
        entry.sig = sig; // nothing conversational yet
        if verbose {
            say(&format!("· {label}: no conversation in it yet"));
        }
        return Ok(0);
    };
    // Stay under Vercel's 4.5 MB body limit: drop the oldest messages if a session is enormous.
    let mut body = upload_body(&t, meta.title.as_deref())?;
    while body.len() > MAX_TRANSCRIPT && t.messages.len() > 1 {
        let drop = (t.messages.len() / 10).max(1);
        t.messages.drain(..drop);
        body = upload_body(&t, meta.title.as_deref())?;
    }
    let res = c
        .put(format!("{}/api/v1/sessions/{id}/transcript", api_base(Some(auth))))
        .bearer_auth(&auth.token)
        .header("content-type", "application/json")
        .body(body.clone())
        .send()
        .await
        .context("can't reach Pine (offline?)")?;
    match res.status().as_u16() {
        200 => {}
        410 => {
            entry.deleted = true; // deleted on the web: stop uploading it
            if verbose {
                say(&format!("· {label}: deleted on the web, skipping it from now on"));
            }
            return Ok(0);
        }
        401 => bail!("This device was signed out of Pine. Run `pn login`."),
        s => {
            let b: Value = res.json().await.unwrap_or(Value::Null);
            bail!("upload failed: {} (HTTP {s})", b["error"].as_str().unwrap_or("error"))
        }
    }
    entry.sig = sig;
    // Checkpoint offsets for transcript sessions: "the first N messages".
    entry.files = BTreeMap::from([("transcript.json".to_string(), t.messages.len() as u64)]);
    if verbose {
        say(&format!("✓ {label}: {} messages, {}", t.messages.len(), human(body.len() as u64)));
    }
    Ok(body.len() as u64)
}

/// Claude Code / Codex sessions sync covers: started since login or already
/// shared, or all with `--all`.
fn native_sessions(all: bool) -> Vec<crate::native::Native> {
    let since = if all { 0 } else { load_auth().map(|a| a.since_ms).unwrap_or(u64::MAX) };
    let shared = load_state();
    crate::native::discover().into_iter().filter(|n| n.created_ms >= since || already_shared(&shared, &n.id)).collect()
}

/// One pass over every local session: pn's recordings, then Claude Code's
/// and Codex's own files.
async fn sync_once(verbose: bool, all: bool) -> Result<(u64, usize)> {
    let auth = require_auth()?;
    let c = client()?;
    let mut state = load_state();
    let mut total = 0;
    let mut result = Ok(());
    let sessions = local_sessions(all)?;
    let natives = native_sessions(all);
    let index = crate::native::NativeIndex::new(&natives);
    // Once: before native-first, natives of pn runs were marked skip. Look again.
    if !state.native_first {
        state.sessions.values_mut().for_each(|s| s.skip = false);
        state.native_first = true;
    }
    // Once: re-send everything so Pine gets each session's own title.
    if !state.titles_sent {
        state.sessions.values_mut().for_each(|s| s.sig = 0);
        state.titles_sent = true;
    }
    let checked = sessions.len() + natives.len();
    let sources = sessions
        .iter()
        .map(|(id, dir)| (id.as_str(), Source::Pine(dir)))
        .chain(natives.iter().map(|n| (n.id.as_str(), Source::Native(n))));
    for (id, src) in sources {
        match sync_session(&c, &auth, &mut state, &index, id, src, verbose).await {
            Ok(n) => total += n,
            Err(e) => {
                result = Err(e);
                break;
            }
        }
    }
    save_state(&state)?;
    result.map(|_| (total, checked))
}

/// `pine sync [--once] [status|pause|resume]`
pub async fn sync_cmd(sub: Option<SyncCmd>, once: bool, all: bool) -> Result<()> {
    let flag = root()?.join("sync-paused");
    match sub {
        Some(SyncCmd::Pause) => {
            std::fs::write(&flag, b"")?;
            println!("Sync paused. Pine keeps recording locally; `pn sync resume` to continue.");
        }
        Some(SyncCmd::Resume) => {
            let _ = std::fs::remove_file(&flag);
            println!("Sync resumed.");
        }
        Some(SyncCmd::Status) => {
            println!("Sync is {}.", if sync_paused() { "paused" } else { "on" });
            println!("Waiting to upload: {}", sessions_label(pending_sessions()?));
        }
        None => {
            let auth = require_auth()?;
            if sync_paused() {
                bail!("Sync is paused. Run `pn sync resume` first.");
            }
            // Say what's covered up front: otherwise an up-to-date sync prints nothing at all.
            let natives = native_sessions(all);
            let claude = natives.iter().filter(|n| n.agent == "claude").count();
            let covered = local_sessions(all)?.len() + natives.len();
            println!("Syncing as @{} to {}", auth.login, auth.api);
            println!(
                "  {} {}: {} from pn, {claude} Claude Code, {} Codex (CLI and desktop apps).",
                sessions_label(covered),
                if all { "on this machine" } else { "since you logged in (add --all for older ones)" },
                covered - natives.len(),
                natives.len() - claude,
            );
            // Each session prints its own lines (sync_session); between them one
            // in-place status line shows the watcher is alive.
            let tty = std::io::IsTerminal::is_terminal(&std::io::stdout());
            let (mut first, mut checks, mut watching) = (true, 0u64, covered);
            loop {
                checks += 1;
                match sync_once(true, all).await {
                    Ok((n, seen)) => {
                        watching = seen;
                        if n == 0 && (first || once) {
                            say("✓ Everything is up to date.");
                        }
                    }
                    Err(e) if once => return Err(e),
                    Err(e) => say(&format!("! {e:#}. Retrying in {}s.", LIVE_EVERY.as_secs())),
                }
                if once {
                    return Ok(());
                }
                if first {
                    say("Watching for new messages. Ctrl-C to stop.");
                    first = false;
                }
                if tty {
                    use std::io::Write;
                    print!("\r\x1b[K· watching {} · checked {checks} times, every {}s", sessions_label(watching), LIVE_EVERY.as_secs());
                    let _ = std::io::stdout().flush();
                }
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => return Ok(()),
                    _ = tokio::time::sleep(LIVE_EVERY) => {}
                }
            }
        }
    }
    Ok(())
}

/// Run a launcher with live sync alongside it (silent: the agent's TUI owns
/// the terminal). Errors are retried on the next tick; nothing is lost.
pub async fn with_live_sync<F: std::future::Future<Output = Result<()>>>(launch: F) -> Result<()> {
    let live = load_auth().is_some() && !sync_paused();
    let task = live.then(|| {
        tokio::spawn(async {
            loop {
                tokio::time::sleep(LIVE_EVERY).await;
                let _ = sync_once(false, false).await;
            }
        })
    });
    let result = launch.await;
    if let Some(t) = task {
        t.abort();
        // Final flush so the end of the session lands too.
        let _ = tokio::time::timeout(Duration::from_secs(20), sync_once(false, false)).await;
    }
    result
}

// ── Checkpoints & pull ──────────────────────────────────────────────────────

fn last_session_id() -> Result<String> {
    let p = root()?.join("last-session");
    let s = std::fs::read_to_string(&p).context("No session yet. Run `pn` first, or pass --session <id>.")?;
    s.lines().next().map(str::to_string).filter(|l| is_ulid(l)).ok_or_else(|| anyhow!("Can't read {}", p.display()))
}

/// `pine checkpoint <name> [-m note] [--session id]`
pub async fn checkpoint(name: String, note: Option<String>, session: Option<String>) -> Result<()> {
    let auth = require_auth()?;
    let id = match session {
        Some(s) => s,
        None => last_session_id()?,
    };
    let c = client()?;
    // Make sure everything up to *now* is uploaded, then freeze those offsets.
    let mut state = load_state();
    let dir = crate::paths::session_dir(&id)?;
    let mut id = id;
    if dir.is_dir() && !dir.join(".pulled").exists() {
        let natives = crate::native::discover();
        let index = crate::native::NativeIndex::new(&natives);
        // A pn run whose conversation Claude Code / Codex saved itself is shared
        // from that file, so the checkpoint goes on that session.
        match index.native_copy(&id, &dir).and_then(|pid| natives.iter().find(|n| n.id == pid)) {
            Some(n) => {
                id = n.id.clone();
                sync_session(&c, &auth, &mut state, &index, &id, Source::Native(n), true).await?;
            }
            None => {
                sync_session(&c, &auth, &mut state, &index, &id, Source::Pine(&dir), true).await?;
            }
        }
        save_state(&state)?;
    }
    let offsets = state.sessions.get(&id).map(|s| json!(s.files));
    let res = api_json(
        c.post(format!("{}/api/v1/checkpoints", api_base(Some(&auth))))
            .bearer_auth(&auth.token)
            .json(&json!({ "session_id": id, "name": name, "note": note, "offsets": offsets })),
    )
    .await?;
    println!("✓ Checkpoint \"{}\" saved for session {id}.", res["name"].as_str().unwrap_or(&name));
    println!("  Teammates with access to the repo can run:  pn pull {name}");
    Ok(())
}

/// `pine checkpoints`
pub async fn checkpoints() -> Result<()> {
    let auth = require_auth()?;
    let res = api_json(client()?.get(format!("{}/api/v1/checkpoints", api_base(Some(&auth)))).bearer_auth(&auth.token)).await?;
    let list = res["checkpoints"].as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        println!("No checkpoints yet. Create one with: pn checkpoint \"name\"");
        return Ok(());
    }
    for c in list {
        println!(
            "{:<28} @{:<14} {:<36} {}",
            c["name"].as_str().unwrap_or(""),
            c["user"]["login"].as_str().unwrap_or(""),
            c["sessionTitle"].as_str().unwrap_or("").chars().take(36).collect::<String>(),
            c["createdAt"].as_str().unwrap_or("").get(..16).unwrap_or("").replace('T', " "),
        );
    }
    Ok(())
}

/// `pine pull <checkpoint-name | session-id>`
pub async fn pull(target: String, into: Option<String>) -> Result<()> {
    let auth = require_auth()?;
    let api = api_base(Some(&auth));
    let c = client()?;
    let (id, files, label): (String, BTreeMap<String, u64>, String) = if is_ulid(&target) {
        let r = api_json(c.get(format!("{api}/api/v1/sessions/{target}")).bearer_auth(&auth.token)).await?;
        (target.clone(), serde_json::from_value(r["files"].clone()).unwrap_or_default(), format!("session {target}"))
    } else {
        let r = api_json(c.get(format!("{api}/api/v1/checkpoints/{target}")).bearer_auth(&auth.token)).await?;
        let sid = r["checkpoint"]["sessionId"].as_str().ok_or_else(|| anyhow!("bad checkpoint response"))?.to_string();
        (sid, serde_json::from_value(r["offsets"].clone()).unwrap_or_default(), format!("checkpoint \"{target}\""))
    };
    // Its repo, to continue in your clone of it.
    let repo = api_json(c.get(format!("{api}/api/v1/sessions/{id}")).bearer_auth(&auth.token)).await.ok().and_then(|r| r["session"]["repo"].as_str().map(String::from));
    let cwd = clone_for(repo.as_deref())?;

    let dest = crate::paths::session_dir(&id)?;
    if dest.is_dir() && !dest.join(".pulled").exists() {
        // Your own session: nothing to download, but you can still continue it
        // in another agent (e.g. a Codex conversation in Claude Code).
        let Some(agent) = into else {
            println!("{} is your own session; it's already at {}", id, dest.display());
            println!("  To continue it in another agent here:  pn pull {target} --into claude|codex");
            return Ok(());
        };
        let t = crate::transcript::build(&dest).ok_or_else(|| anyhow!("no conversation in {id}"))?;
        return continue_in(&serde_json::to_value(t)?, &agent, &label, &cwd);
    }
    for (file, upto) in &files {
        // Transcript sessions: one JSON file, cut to the first `upto` messages for a checkpoint.
        let url = if file == "transcript.json" {
            format!("{api}/api/v1/sessions/{id}/transcript?upto={upto}")
        } else {
            format!("{api}/api/v1/sessions/{id}/file?path={file}&upto={upto}")
        };
        let bytes = c
            .get(url)
            .bearer_auth(&auth.token)
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;
        let out = dest.join(file);
        if let Some(p) = out.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::write(&out, &bytes)?;
    }
    std::fs::write(
        dest.join(".pulled"),
        serde_json::to_vec_pretty(&json!({ "from": label, "pulled_at": now_rfc3339(), "api": api }))?,
    )?;
    println!("✓ Pulled {label} ({} files) → {}", files.len(), dest.display());
    let Ok(raw) = std::fs::read(dest.join("transcript.json")) else {
        return Ok(()); // an old raw-synced session: no transcript to continue from
    };
    let t: Value = serde_json::from_slice(&raw)?;
    let agent = into.unwrap_or_else(|| t["agent"].as_str().unwrap_or("claude").to_string());
    continue_in(&t, &agent, &label, &cwd)
}

/// Where to continue a session from `repo`: this folder if it's a clone of
/// it, else the clone Claude Code last used for it (`githubRepoPaths` in
/// ~/.claude.json), else this folder.
fn clone_for(repo: Option<&str>) -> Result<PathBuf> {
    let here = std::env::current_dir()?;
    let Some(repo) = repo else { return Ok(here) };
    let same = |p: &Path| {
        git(&p.to_string_lossy(), &["remote", "get-url", "origin"]).and_then(|r| github_repo(&r)).is_some_and(|r| r.eq_ignore_ascii_case(repo))
    };
    if same(&here) {
        return Ok(here);
    }
    let known: Value = dirs::home_dir()
        .and_then(|h| std::fs::read(h.join(".claude.json")).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null);
    let paths = known["githubRepoPaths"].as_object().and_then(|m| m.iter().find(|(k, _)| k.eq_ignore_ascii_case(repo)).map(|(_, v)| v.clone()));
    if let Some(p) = paths.and_then(|v| v.as_array().cloned()).into_iter().flatten().filter_map(|v| v.as_str().map(PathBuf::from)).find(|p| p.is_dir()) {
        println!("Using your clone of {repo}: {}", p.display());
        return Ok(p);
    }
    println!("Note: this folder isn't a clone of {repo}. Continuing here; cd into your clone and pull again to switch.");
    Ok(here)
}

/// Ask once, in a terminal only.
fn yes(prompt: &str) -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal() && ask(prompt).is_some_and(|a| a.is_empty() || a.eq_ignore_ascii_case("y"))
}

/// Hand a pulled transcript to an agent in `cwd` as a native session, and offer to open it.
fn continue_in(t: &Value, agent: &str, label: &str, cwd: &Path) -> Result<()> {
    let cd = if std::env::current_dir().is_ok_and(|h| h == cwd) { String::new() } else { format!("cd {} && ", cwd.display()) };
    let args: Vec<String> = match agent {
        "claude" => {
            let sid = crate::resume::into_claude(t, label, cwd)?;
            println!("✓ Added to Claude Code for {}", cwd.display());
            println!("  Continue it:  {cd}pn -- --resume {sid}     (or: {cd}claude --resume {sid})");
            vec!["--".into(), "--resume".into(), sid]
        }
        "codex" => {
            let title = format!("From Pine: {label}");
            let id = crate::into_codex::write_codex(crate::resume::turns(t, label), &title, cwd)?;
            println!("✓ Added to Codex for {}", cwd.display());
            println!("  Continue it:  {cd}pn codex -- resume {id}     (or: {cd}codex resume {id})");
            println!("  Codex app:    open codex://threads/{id}");
            vec!["codex".into(), "--".into(), "resume".into(), id]
        }
        other => bail!("--into takes claude or codex, not {other}"),
    };
    if yes("Open it now? [Y/n] ") {
        // Through pn, so the continued conversation is recorded too.
        let status = std::process::Command::new(std::env::current_exe()?).args(&args).current_dir(cwd).status()?;
        std::process::exit(status.code().unwrap_or(0));
    }
    Ok(())
}

/// `pine open [id]`: the session in the browser.
pub fn open(id: Option<String>) -> Result<()> {
    let id = match id {
        Some(i) => i,
        None => last_session_id()?,
    };
    let url = format!("{}/sessions/{id}", api_base(load_auth().as_ref()));
    println!("{url}");
    open_browser(&url);
    Ok(())
}

fn human(n: u64) -> String {
    match n {
        n if n >= 1 << 20 => format!("{:.1} MB", n as f64 / (1u64 << 20) as f64),
        n if n >= 1 << 10 => format!("{:.0} KB", n as f64 / 1024.0),
        n => format!("{n} B"),
    }
}

#[cfg(test)]
mod tests {
    use super::github_repo;

    #[test]
    fn parses_github_remotes() {
        assert_eq!(github_repo("git@github.com:acme/api.git").as_deref(), Some("acme/api"));
        assert_eq!(github_repo("https://github.com/acme/api").as_deref(), Some("acme/api"));
        assert_eq!(github_repo("https://github.com/acme/api.git/").as_deref(), Some("acme/api"));
        assert_eq!(github_repo("ssh://git@github.com/acme/api.git").as_deref(), Some("acme/api"));
        assert_eq!(github_repo("https://gitlab.com/acme/api"), None);
    }
}

// ── Interactive menu (`pine -i`) ─────────────────────────────────────────────

/// Read one trimmed line; None on EOF (Ctrl-D).
pub(crate) fn ask(prompt: &str) -> Option<String> {
    use std::io::Write;
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    match std::io::stdin().read_line(&mut line) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(line.trim().to_string()),
    }
}

/// Recent sessions recorded on this machine, newest first: (id, folder/repo label, size).
fn recent_local(n: usize) -> Vec<(String, String, u64)> {
    let mut all = local_sessions_since(0).unwrap_or_default();
    all.reverse();
    all.into_iter()
        .take(n)
        .map(|(id, dir)| {
            let cwd = std::fs::read(dir.join("meta.json"))
                .ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                .and_then(|m| m["cwd"].as_str().map(String::from))
                .unwrap_or_default();
            let label = Path::new(&cwd).file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "—".into());
            let size = session_files(&dir).iter().map(|(_, l)| l).sum();
            (id, label, size)
        })
        .collect()
}

fn started(id: &str) -> String {
    id.parse::<ulid::Ulid>()
        .ok()
        .and_then(|u| time::OffsetDateTime::from_unix_timestamp((u.timestamp_ms() / 1000) as i64).ok())
        .map(|t| format!("{}-{:02}-{:02} {:02}:{:02} UTC", t.year(), t.month() as u8, t.day(), t.hour(), t.minute()))
        .unwrap_or_default()
}

/// `pine -i`: a small menu for login, sync, checkpoints and sessions.
pub async fn interactive() -> Result<()> {
    loop {
        let auth = load_auth();
        let who = auth.as_ref().map(|a| format!("@{} ({})", a.login, a.email)).unwrap_or_else(|| "not logged in · recording locally only".into());
        let sync = if auth.is_none() {
            "off".to_string()
        } else if sync_paused() {
            "paused".to_string()
        } else {
            format!("on · {} waiting", sessions_label(pending_sessions().unwrap_or(0)))
        };
        println!("\n  Pine  {who}\n  Sync  {sync}\n");
        println!("  1  Who am I");
        println!("  2  {}", if auth.is_some() { "Log out" } else { "Log in with GitHub" });
        println!("  3  Sync now");
        println!("  4  {} sync", if sync_paused() { "Resume" } else { "Pause" });
        println!("  5  Team checkpoints  (pull one)");
        println!("  6  My recent sessions  (open / checkpoint)");
        println!("  7  Open Pine in the browser");
        println!("  q  Quit\n");
        let Some(choice) = ask("  > ") else { return Ok(()) };
        let result = match choice.as_str() {
            "1" => whoami().await,
            "2" if auth.is_some() => logout().await,
            "2" => login().await,
            "3" => sync_cmd(None, true, false).await,
            "4" => sync_cmd(Some(if sync_paused() { SyncCmd::Resume } else { SyncCmd::Pause }), false, false).await,
            "5" => pick_checkpoint().await,
            "6" => pick_session().await,
            "7" => {
                let url = format!("{}/sessions", api_base(auth.as_ref()));
                open_browser(&url);
                println!("  {url}");
                Ok(())
            }
            "q" | "Q" | "quit" | "exit" => return Ok(()),
            "" => Ok(()),
            _ => {
                println!("  Pick 1–7 or q.");
                Ok(())
            }
        };
        if let Err(e) = result {
            println!("  ✗ {e}");
        }
    }
}

async fn pick_checkpoint() -> Result<()> {
    let auth = require_auth()?;
    let res = api_json(client()?.get(format!("{}/api/v1/checkpoints", api_base(Some(&auth)))).bearer_auth(&auth.token)).await?;
    let list = res["checkpoints"].as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        println!("  No checkpoints yet. Make one from \"My recent sessions\".");
        return Ok(());
    }
    for (i, c) in list.iter().take(20).enumerate() {
        println!(
            "  {:>2}  {:<26} @{:<14} {}",
            i + 1,
            c["name"].as_str().unwrap_or(""),
            c["user"]["login"].as_str().unwrap_or(""),
            c["sessionTitle"].as_str().unwrap_or("").chars().take(40).collect::<String>()
        );
    }
    let Some(pick) = ask("\n  Pull which? (number, Enter to go back) > ") else { return Ok(()) };
    let Some(c) = pick.parse::<usize>().ok().and_then(|n| list.get(n.wrapping_sub(1))) else { return Ok(()) };
    pull(c["name"].as_str().unwrap_or_default().to_string(), None).await
}

async fn pick_session() -> Result<()> {
    let sessions = recent_local(10);
    if sessions.is_empty() {
        println!("  No sessions yet. Run `pn` to start one.");
        return Ok(());
    }
    for (i, (id, label, size)) in sessions.iter().enumerate() {
        println!("  {:>2}  {}  {:<20} {:>8}  {id}", i + 1, started(id), label, human(*size));
    }
    let Some(pick) = ask("\n  Which session? (number, Enter to go back) > ") else { return Ok(()) };
    let Some((id, _, _)) = pick.parse::<usize>().ok().and_then(|n| sessions.get(n.wrapping_sub(1))) else { return Ok(()) };
    let Some(action) = ask("  [o]pen in browser, [c]heckpoint it, Enter to go back > ") else { return Ok(()) };
    match action.as_str() {
        "o" => open(Some(id.clone())),
        "c" => {
            let Some(name) = ask("  Checkpoint name > ").filter(|s| !s.is_empty()) else { return Ok(()) };
            let note = ask("  Note (optional) > ").filter(|s| !s.is_empty());
            checkpoint(name, note, Some(id.clone())).await
        }
        _ => Ok(()),
    }
}
