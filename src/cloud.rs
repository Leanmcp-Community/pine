//! Optional file-backed cloud uploader. Never modifies captured sessions.
use anyhow::{bail, Context, Result};
use clap::Subcommand;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Enable cloud uploads. Key is read from PINE_UPLOAD_KEY.
    Configure {
        #[arg(long)]
        endpoint: String,
        #[arg(long)]
        backfill: bool,
    },
    Status,
    /// Stop the running uploader after its current request (up to 30 seconds).
    Stop,
    Disable,
    /// Sync all existing and new sessions continuously, or make one pass with --once.
    Sync {
        #[arg(long)]
        once: bool,
        /// Include historical sessions (now the default; accepted for compatibility).
        #[arg(long)]
        backfill: bool,
    },
}
#[derive(Serialize, Deserialize)]
struct Config {
    endpoint: String,
    key: String,
    device: String,
    since: String,
    enabled: bool,
}
#[derive(Default, Serialize, Deserialize)]
struct State {
    offsets: BTreeMap<String, u64>,
    metadata: BTreeMap<String, String>,
    last_success: Option<String>,
}
fn file(name: &str) -> Result<PathBuf> {
    Ok(crate::paths::root()?.join(name))
}
fn save<T: Serialize>(path: &Path, data: &T) -> Result<()> {
    use std::io::Write;
    std::fs::create_dir_all(path.parent().unwrap())?;
    let tmp = path.with_extension(format!("{}.tmp", ulid::Ulid::new()));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp)?;
    f.write_all(&serde_json::to_vec_pretty(data)?)?;
    f.sync_all()?;
    std::fs::rename(tmp, path)?;
    Ok(())
}
fn config() -> Result<Config> {
    Ok(serde_json::from_slice(
        &std::fs::read(file("cloud.json")?).context("Run pine cloud configure first")?,
    )?)
}
fn state() -> Result<State> {
    let p = file("cloud-state.json")?;
    if p.exists() {
        Ok(serde_json::from_slice(&std::fs::read(p)?)?)
    } else {
        Ok(State::default())
    }
}
fn digest(b: &[u8]) -> String {
    hex::encode(Sha256::digest(b))
}
fn sensitive(k: &str) -> bool {
    matches!(
        k.to_ascii_lowercase().as_str(),
        "authorization"
            | "proxy-authorization"
            | "x-api-key"
            | "api_key"
            | "apikey"
            | "access_token"
            | "refresh_token"
            | "cookie"
            | "set-cookie"
    )
}
fn sanitize(v: &mut Value) {
    match v {
        Value::Object(m) => {
            for (k, v) in m {
                if sensitive(k) {
                    *v = Value::String("<redacted>".into());
                } else {
                    sanitize(v);
                }
            }
        }
        Value::Array(a) => {
            if a.len() == 2 && a[0].as_str().is_some_and(sensitive) {
                a[1] = Value::String("<redacted>".into());
            } else {
                for v in a {
                    sanitize(v);
                }
            }
        }
        _ => {}
    }
}
/// Counts source bytes acknowledged by the server, not filtered wire bytes.
struct Progress {
    session: String,
    total: u64,
    done: u64,
    active: bool,
    terminal: bool,
}
impl Progress {
    fn show(&mut self, file: &str) {
        use std::io::{IsTerminal, Write};
        self.terminal = std::io::stderr().is_terminal();
        self.active = true;
        let fraction = if self.total == 0 {
            1.0
        } else {
            self.done.min(self.total) as f64 / self.total as f64
        };
        let filled = (fraction * 20.0) as usize;
        let line = format!(
            "Session {} [{}{}] {:3.0}%  {} / {} source bytes  {}",
            self.session,
            "=".repeat(filled),
            "-".repeat(20 - filled),
            fraction * 100.0,
            self.done.min(self.total),
            self.total,
            file
        );
        if self.terminal {
            eprint!("\r\x1b[2K{line}");
            let _ = std::io::stderr().flush();
        } else {
            eprintln!("{line}");
        }
    }
    fn finish(&mut self) {
        if self.active {
            let status = if self.done >= self.total {
                "Caught up to this scan"
            } else {
                "Pending bytes remain; next scan will continue"
            };
            self.show(status);
            if self.terminal {
                eprintln!();
            }
            self.active = false;
        }
    }
}
impl Drop for Progress {
    fn drop(&mut self) {
        if self.active && self.terminal {
            eprintln!();
        }
    }
}
pub async fn run(cmd: Command) -> Result<()> {
    match cmd {
        Command::Configure { endpoint, backfill } => {
            let u = reqwest::Url::parse(&endpoint)?;
            if u.scheme() != "https"
                && !(u.scheme() == "http"
                    && matches!(u.host_str(), Some("localhost" | "127.0.0.1" | "[::1]")))
            {
                bail!("Remote endpoints require HTTPS");
            }
            if !u.username().is_empty()
                || u.password().is_some()
                || u.query().is_some()
                || u.fragment().is_some()
                || u.path() != "/"
            {
                bail!("Use an origin URL without credentials, path, query or fragment");
            }
            let key = std::env::var("PINE_UPLOAD_KEY")
                .context("Set PINE_UPLOAD_KEY to your organization upload key")?;
            if key.len() < 32 {
                bail!("Upload key must contain at least 32 characters");
            }
            let old = if file("cloud.json")?.exists() {
                Some(config()?)
            } else {
                None
            };
            if old
                .as_ref()
                .is_some_and(|c| c.endpoint != endpoint.trim_end_matches('/'))
            {
                bail!("Changing servers requires a separate PINE_HOME to keep checkpoints isolated");
            }
            let c = Config {
                endpoint: endpoint.trim_end_matches('/').into(),
                key,
                device: old
                    .as_ref()
                    .map(|c| c.device.clone())
                    .unwrap_or_else(|| ulid::Ulid::new().to_string()),
                since: if backfill {
                    String::new()
                } else {
                    old.map(|c| c.since)
                        .unwrap_or_else(|| ulid::Ulid::new().to_string())
                },
                enabled: true,
            };
            save(&file("cloud.json")?, &c)?;
            println!("Cloud enabled. Captured prompts, bodies and machine metadata will be uploaded. Structured credentials are filtered; arbitrary content may contain secrets. Run pine cloud sync.");
        }
        Command::Stop => {
            #[cfg(unix)]
            {
                use std::os::fd::AsRawFd;
                let path = file("cloud-sync.lock")?;
                if !path.exists() {
                    println!("Uploader is already stopped.");
                    return Ok(());
                }
                let lock = std::fs::OpenOptions::new().write(true).open(path)?;
                let stopped =
                    || unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 };
                if stopped() {
                    println!("Uploader is already stopped.");
                    return Ok(());
                }
                save(&file("cloud-stop.json")?, &true)?;
                println!("Stopping uploader; waiting for the current request...");
                for _ in 0..70 {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    if stopped() {
                        println!("Uploader stopped. Checkpoints preserved.");
                        return Ok(());
                    }
                }
                bail!("Stop requested, but worker has not exited after 35 seconds. Inspect its service logs.");
            }
            #[cfg(not(unix))]
            {
                save(&file("cloud-stop.json")?, &true)?;
                println!("Stop requested.");
            }
        }
        Command::Disable => {
            let mut c = config()?;
            c.enabled = false;
            save(&file("cloud.json")?, &c)?;
            println!("Cloud disabled; local capture continues.");
        }
        Command::Status => {
            let c = config()?;
            let s = state()?;
            println!(
                "{}\nEnabled: {}\nDevice: {}\nTracked files: {}\nLast successful pass: {}",
                c.endpoint,
                c.enabled,
                c.device,
                s.offsets.len() + s.metadata.len(),
                s.last_success.as_deref().unwrap_or("never")
            );
        }
        Command::Sync { once, backfill: _ } => {
            // Advisory lock automatically releases after crashes.
            let lock_path = file("cloud-sync.lock")?;
            std::fs::create_dir_all(lock_path.parent().unwrap())?;
            let lock = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(lock_path)?;
            #[cfg(unix)]
            {
                use std::os::fd::AsRawFd;
                if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                    bail!("Another uploader is running");
                }
            }
            let stop_path = file("cloud-stop.json")?;
            if stop_path.exists() {
                std::fs::remove_file(&stop_path)?;
            }
            let client = reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(30))
                .build()?;
            eprintln!(
                "Cloud sync starting. Progress counts acknowledged source bytes per session."
            );
            eprintln!(
                "Syncing existing and new sessions allowed by the administrator policy. Already acknowledged data is skipped."
            );
            let mut idle = false;
            let mut delay = 2;
            loop {
                if stop_path.exists() {
                    eprintln!("Uploader stopped.");
                    break;
                }
                let c = config()?;
                if !c.enabled {
                    println!("Cloud disabled");
                    break;
                }
                let result = sync(&client, &c).await;
                if once {
                    return result.map(|uploaded| {
                        if !uploaded {
                            eprintln!("No new complete data to upload in eligible sessions.");
                        }
                    });
                }
                match result {
                    Ok(uploaded) => {
                        delay = 2;
                        if uploaded || !idle {
                            eprintln!("Watching for new data (every 2s). Ctrl-C to stop.");
                        }
                        idle = true;
                    }
                    Err(e) => {
                        idle = false;
                        eprintln!("Cloud sync paused: {e:#}. Retrying.");
                        delay = (delay * 2).min(60);
                    }
                }
                tokio::select! { _=tokio::signal::ctrl_c()=>break, _=tokio::time::sleep(Duration::from_secs(delay))=>{} }
            }
        }
    }
    Ok(())
}
#[derive(Deserialize)]
struct Policy {
    sync_enabled: bool,
    max_session_bytes: u64,
    max_chunk_bytes: u64,
}

// Count logical file bytes, never following symlinks. Stop as soon as over limit.
fn session_size(dir: &Path, limit: u64) -> Result<u64> {
    let mut total = 0u64;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let meta = entry.path().symlink_metadata()?;
        if meta.is_file() {
            total = total.saturating_add(meta.len());
        } else if meta.is_dir() {
            total = total.saturating_add(session_size(&entry.path(), limit)?);
        }
        if total >= limit {
            break;
        }
    }
    Ok(total)
}

async fn sync(client: &reqwest::Client, c: &Config) -> Result<bool> {
    use std::io::{Read, Seek, SeekFrom};
    let response = client
        .get(format!("{}/v1/config", c.endpoint))
        .bearer_auth(&c.key)
        .send()
        .await
        .context("Cannot fetch administrator policy; uploads paused")?;
    if !response.status().is_success() {
        bail!("Cannot fetch administrator policy: {}", response.status());
    }
    let policy: Policy = response.json().await?;
    if !policy.sync_enabled {
        return Ok(false);
    }
    let limit = policy.max_session_bytes;
    if limit == 0 || policy.max_chunk_bytes < 2 {
        bail!("Invalid administrator upload limits");
    }
    // Leave room for redaction/serialization expansion in structured logs.
    let chunk_limit = (policy.max_chunk_bytes / 2).min(4 * 1024 * 1024);
    let mut s = state()?;
    let mut uploaded = false;
    let root = crate::paths::sessions_dir()?;
    if !root.exists() {
        return Ok(false);
    }
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let session = entry.file_name().to_string_lossy().to_string();
        if session.parse::<ulid::Ulid>().is_err() {
            continue;
        }
        if session_size(&entry.path(), limit)? >= limit {
            static REPORTED: std::sync::OnceLock<
                std::sync::Mutex<std::collections::HashSet<String>>,
            > = std::sync::OnceLock::new();
            if REPORTED
                .get_or_init(Default::default)
                .lock()
                .unwrap()
                .insert(session.clone())
            {
                eprintln!(
                    "Skipping session {session}: size is at least the server limit of {} MiB (local files preserved).", limit / 1048576
                );
            }
            continue;
        }
        let mut files = vec![
            entry.path().join("meta.json"),
            entry.path().join("traffic.jsonl"),
        ];
        for folder in ["bodies", "stream"] {
            let p = entry.path().join(folder);
            if p.symlink_metadata()
                .is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink())
            {
                for f in std::fs::read_dir(p)? {
                    files.push(f?.path());
                }
            }
        }
        let mut progress = Progress {
            session: session.clone(),
            total: 0,
            done: 0,
            active: false,
            terminal: false,
        };
        for p in &files {
            if let Ok(m) = p.symlink_metadata() {
                if m.is_file() && !m.file_type().is_symlink() {
                    let rel = p.strip_prefix(entry.path())?.to_string_lossy().to_string();
                    if rel == "meta.json" || rel.ends_with(".jsonl") || rel.ends_with(".bin") {
                        progress.total += m.len();
                        if rel != "meta.json" {
                            progress.done += s
                                .offsets
                                .get(&format!("{session}/{rel}"))
                                .copied()
                                .unwrap_or(0)
                                .min(m.len());
                        }
                    }
                }
            }
        }
        for p in files {
            if file("cloud-stop.json")?.exists() {
                return Ok(uploaded);
            }
            if !p
                .symlink_metadata()
                .is_ok_and(|m| m.is_file() && !m.file_type().is_symlink())
            {
                continue;
            }
            let rel = p.strip_prefix(entry.path())?.to_string_lossy().to_string();
            if !(rel == "meta.json" || rel.ends_with(".jsonl") || rel.ends_with(".bin")) {
                continue;
            }
            let id = format!("{session}/{rel}");
            let meta = rel == "meta.json";
            let offset = if meta {
                0
            } else {
                *s.offsets.get(&id).unwrap_or(&0)
            };
            let mut f = std::fs::File::open(&p)?;
            f.seek(SeekFrom::Start(offset))?;
            let mut bytes = Vec::new();
            f.take(chunk_limit).read_to_end(&mut bytes)?;
            if bytes.is_empty() {
                continue;
            }
            if rel.ends_with(".jsonl") {
                if let Some(n) = bytes.iter().rposition(|b| *b == b'\n') {
                    bytes.truncate(n + 1)
                } else {
                    if bytes.len() as u64 == chunk_limit {
                        bail!("Log line exceeds administrator chunk limit in {id}");
                    }
                    continue;
                }
            }
            let consumed = bytes.len() as u64;
            if meta {
                let Ok(mut v) = serde_json::from_slice::<Value>(&bytes) else {
                    continue;
                };
                sanitize(&mut v);
                bytes = serde_json::to_vec(&v)?;
            } else if rel.ends_with(".jsonl") {
                let mut out = Vec::new();
                for line in bytes.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
                    let mut v: Value = serde_json::from_slice(line)
                        .with_context(|| format!("Invalid JSON in {id}"))?;
                    sanitize(&mut v);
                    serde_json::to_writer(&mut out, &v)?;
                    out.push(b'\n');
                }
                bytes = out;
            }
            let checksum = digest(&bytes);
            if meta && s.metadata.get(&id) == Some(&checksum) {
                progress.done += consumed;
                continue;
            }
            progress.show(&format!("Uploading {rel}"));
            let response = client
                .post(format!("{}/v1/chunks", c.endpoint))
                .bearer_auth(&c.key)
                .header("X-Device", &c.device)
                .header("X-Session", &session)
                .header("X-Path", &rel)
                .header("X-Offset", offset)
                .header("X-Source-Length", consumed)
                .header("X-SHA256", &checksum)
                .header("X-Hostname", crate::hostname().unwrap_or_default())
                .body(bytes)
                .send()
                .await
                .context("Upload connection failed")?;
            if !response.status().is_success() {
                bail!("Server returned {} for {id}", response.status())
            }
            if meta {
                s.metadata.insert(id, checksum);
            } else {
                s.offsets.insert(id, offset + consumed);
            }
            save(&file("cloud-state.json")?, &s)?;
            uploaded = true;
            progress.done += consumed;
            progress.show(&format!("Acknowledged {rel}"));
        }
        progress.finish();
    }
    s.last_success = Some(time::OffsetDateTime::now_utc().to_string());
    save(&file("cloud-state.json")?, &s)?;
    Ok(uploaded)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn size_limit_includes_nested_files_and_exact_boundary() {
        let dir = std::env::temp_dir().join(format!("pine-size-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(dir.join("bodies")).unwrap();
        let f = std::fs::File::create(dir.join("bodies/large.bin")).unwrap();
        f.set_len(200 * 1024 * 1024 - 1).unwrap();
        assert!(session_size(&dir, 200 * 1024 * 1024).unwrap() < 200 * 1024 * 1024);
        std::fs::write(dir.join("meta.json"), b"x").unwrap();
        assert!(session_size(&dir, 200 * 1024 * 1024).unwrap() >= 200 * 1024 * 1024);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn filters_nested_credentials() {
        let mut v = serde_json::json!({"headers":[["Authorization","secret"],["Accept","ok"]],"nested":{"api_key":"secret"}});
        sanitize(&mut v);
        assert_eq!(v["headers"][0][1], "<redacted>");
        assert_eq!(v["headers"][1][1], "ok");
        assert!(!v.to_string().contains("secret"));
    }
}
