//! Append-only session logger.
//!
//! IMPORTANT: this module never deletes, rotates, truncates, or otherwise
//! removes any captured data on disk. Once a byte lands here it stays.
//! There is no `--max-session-bytes`, no expiry, no cleanup task.
//!
//! Layout under `~/.pine/sessions/<ulid>/`:
//!
//! ```text
//! meta.json                       # session metadata (claude argv, pid, start ts)
//! traffic.jsonl                   # one JSON record per request/response pair
//! bodies/<req-id>.req.bin         # request body if larger than inline threshold
//! bodies/<req-id>.res.bin         # response body if non-streaming
//! stream/<req-id>.sse.jsonl       # per-event log for SSE responses
//! ```

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

use crate::paths;

/// Headers that should be hashed instead of written verbatim when redaction
/// is on (the default).
const REDACT_HEADERS: &[&str] = &[
    "authorization",
    "x-api-key",
    "cookie",
    "set-cookie",
    "proxy-authorization",
];

/// Bodies <= this many bytes are inlined into `traffic.jsonl`. Larger bodies
/// spill to `bodies/<id>.{req,res}.bin`. The threshold only affects layout,
/// never retention.
const INLINE_BODY_LIMIT: usize = 64 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub session_id: String,
    /// Which child app this session is taping: "claude", "windsurf", or "proxy".
    #[serde(default = "default_target")]
    pub target: String,
    #[serde(with = "time::serde::rfc3339")]
    pub started_at: OffsetDateTime,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub ended_at: Option<OffsetDateTime>,
    pub claude_argv: Vec<String>,
    pub claude_path: String,
    pub claude_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_exit_code: Option<i32>,
    /// The Claude Code conversation this run is on (the latest one seen in
    /// `x-claude-code-session-id`). `None` until the first API request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_session_id: Option<String>,
    /// Every Claude Code conversation this run touched, in order: a resume or
    /// `/clear` switches to another one. Sync uses these to prefer Claude
    /// Code's own file for the same conversation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub claude_session_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_version: Option<String>,
    pub proxy_addr: String,
    pub redact: bool,
    pub host_filter: Vec<String>,
    pub pine_version: String,
    /// Absolute path to this session's directory under `~/.pine/sessions/`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_cert_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub term: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<SystemInfo>,
}

fn default_target() -> String {
    "claude".to_string()
}

/// Static host facts gathered once at session start. None of these require
/// elevated privileges; they come from `sysctl`/`/proc`/`uname` and the
/// standard library.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_logical: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_physical: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_brand: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_memory_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_size_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kernel_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os_release: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TrafficRecord {
    pub id: String,
    #[serde(with = "time::serde::rfc3339")]
    pub ts_start: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub ts_end: OffsetDateTime,
    pub method: String,
    pub url: String,
    pub http_version: String,
    pub upstream_addr: Option<String>,
    pub request: SidePayload,
    pub response: ResponsePayload,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SidePayload {
    pub headers: Vec<[String; 2]>,
    pub body_size: usize,
    pub body_inline: Option<InlineBody>,
    pub body_path: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResponsePayload {
    pub status: u16,
    pub headers: Vec<[String; 2]>,
    pub body_size: usize,
    pub body_inline: Option<InlineBody>,
    pub body_path: Option<String>,
    pub is_stream: bool,
    pub stream_path: Option<String>,
    #[serde(default)]
    pub is_websocket: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ws_path: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "encoding", content = "data")]
pub enum InlineBody {
    #[serde(rename = "utf8")]
    Utf8(String),
    #[serde(rename = "base64")]
    Base64(String),
}

impl InlineBody {
    pub fn from_bytes(bytes: &[u8]) -> Self {
        match std::str::from_utf8(bytes) {
            Ok(s) => InlineBody::Utf8(s.to_string()),
            Err(_) => InlineBody::Base64(b64::encode(bytes)),
        }
    }

    pub fn to_b64(&self) -> String {
        match self {
            InlineBody::Utf8(s) => b64::encode(s.as_bytes()),
            InlineBody::Base64(s) => s.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SseEvent {
    #[serde(with = "time::serde::rfc3339")]
    pub ts: OffsetDateTime,
    pub event: Option<String>,
    pub id: Option<String>,
    pub retry: Option<u64>,
    pub data: String,
}

/// Owns the session directory and the locked traffic.jsonl writer.
///
/// Append-only by design. The file is opened with `create(true).append(true)`
/// and never truncated. Crashes leave a valid prefix.
pub struct SessionLogger {
    dir: PathBuf,
    bodies_dir: PathBuf,
    stream_dir: PathBuf,
    traffic: Mutex<tokio::fs::File>,
    redact: bool,
    #[allow(dead_code)]
    pub session_id: String,
    /// Live, in-memory copy of the on-disk `meta.json`. Mutated and rewritten
    /// when fields like `claude_session_id` or `ended_at` become known.
    meta: Mutex<Option<SessionMeta>>,
    /// Claude Code conversation ids already written to meta.json.
    claude_ids_seen: std::sync::Mutex<std::collections::HashSet<String>>,
}

impl SessionLogger {
    pub async fn create(session_id: String, redact: bool) -> Result<Arc<Self>> {
        let dir = paths::session_dir(&session_id)?;
        paths::ensure_dir(&dir)?;
        let bodies_dir = dir.join("bodies");
        paths::ensure_dir(&bodies_dir)?;
        let stream_dir = dir.join("stream");
        paths::ensure_dir(&stream_dir)?;

        let traffic_path = dir.join("traffic.jsonl");
        let traffic = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&traffic_path)
            .await
            .with_context(|| format!("opening {}", traffic_path.display()))?;

        Ok(Arc::new(Self {
            dir,
            bodies_dir,
            stream_dir,
            traffic: Mutex::new(traffic),
            redact,
            session_id,
            meta: Mutex::new(None),
            claude_ids_seen: std::sync::Mutex::new(std::collections::HashSet::new()),
        }))
    }

    pub async fn write_meta(&self, meta: &SessionMeta) -> Result<()> {
        let path = self.dir.join("meta.json");
        let bytes = serde_json::to_vec_pretty(meta)?;
        tokio::fs::write(&path, &bytes)
            .await
            .with_context(|| format!("writing {}", path.display()))?;
        *self.meta.lock().await = Some(meta.clone());
        Ok(())
    }

    /// Apply `f` to the in-memory meta and rewrite `meta.json` if it changed.
    /// Silently no-ops if `write_meta` was never called.
    pub async fn update_meta<F>(&self, f: F) -> Result<()>
    where
        F: FnOnce(&mut SessionMeta),
    {
        let mut guard = self.meta.lock().await;
        let Some(meta) = guard.as_mut() else {
            return Ok(());
        };
        f(meta);
        let path = self.dir.join("meta.json");
        let bytes = serde_json::to_vec_pretty(meta)?;
        tokio::fs::write(&path, &bytes)
            .await
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    /// Record which Claude Code conversation a request belongs to.
    ///
    /// Claude Code sends it on every API call as `x-claude-code-session-id`
    /// (and inside `metadata.user_id` as JSON `{"session_id": ...}`). The
    /// first UUID in `user_id` is the account, not the conversation, so only
    /// these two are trusted. Each new id is appended to meta.json once.
    pub async fn observe_request(&self, headers: &http::HeaderMap, body: &[u8]) {
        let from_header = headers
            .get("x-claude-code-session-id")
            .and_then(|v| v.to_str().ok())
            .and_then(find_uuid);
        let id = from_header.or_else(|| {
            if body.is_empty() || body.len() > 2 * 1024 * 1024 {
                return None;
            }
            let v = serde_json::from_slice::<serde_json::Value>(body).ok()?;
            let user_id = v.get("metadata")?.get("user_id")?.as_str()?;
            let inner = serde_json::from_str::<serde_json::Value>(user_id).ok()?;
            find_uuid(inner.get("session_id")?.as_str()?)
        });
        let Some(id) = id else { return };
        if !self.claude_ids_seen.lock().map(|mut s| s.insert(id.clone())).unwrap_or(false) {
            return;
        }
        if let Err(e) = self
            .update_meta(|m| {
                if !m.claude_session_ids.contains(&id) {
                    m.claude_session_ids.push(id.clone());
                }
                m.claude_session_id = Some(id);
            })
            .await
        {
            tracing::warn!(error = %e, "updating meta.json with claude_session_ids");
        }
    }

    #[allow(dead_code)]
    pub fn redact(&self) -> bool {
        self.redact
    }

    #[allow(dead_code)]
    pub fn session_dir(&self) -> &std::path::Path {
        &self.dir
    }

    pub fn redact_headers(&self, headers: &[(String, Vec<u8>)]) -> Vec<[String; 2]> {
        headers
            .iter()
            .map(|(k, v)| {
                let val = if self.redact && REDACT_HEADERS.iter().any(|r| r.eq_ignore_ascii_case(k))
                {
                    let mut h = Sha256::new();
                    h.update(v);
                    let digest = hex::encode(h.finalize());
                    format!("<redacted:sha256:{}>", &digest[..16])
                } else {
                    String::from_utf8_lossy(v).to_string()
                };
                [k.clone(), val]
            })
            .collect()
    }

    /// Persist a request body. Returns (size, optional path, optional inline payload).
    pub async fn store_request_body(
        &self,
        req_id: &str,
        body: &[u8],
    ) -> Result<(usize, Option<String>, Option<InlineBody>)> {
        if body.is_empty() {
            return Ok((0, None, None));
        }
        if body.len() <= INLINE_BODY_LIMIT {
            return Ok((body.len(), None, Some(InlineBody::from_bytes(body))));
        }
        let rel = format!("bodies/{req_id}.req.bin");
        let path = self.bodies_dir.join(format!("{req_id}.req.bin"));
        write_append(&path, body).await?;
        Ok((body.len(), Some(rel), None))
    }

    pub async fn store_response_body(
        &self,
        req_id: &str,
        body: &[u8],
    ) -> Result<(usize, Option<String>, Option<InlineBody>)> {
        if body.is_empty() {
            return Ok((0, None, None));
        }
        if body.len() <= INLINE_BODY_LIMIT {
            return Ok((body.len(), None, Some(InlineBody::from_bytes(body))));
        }
        let rel = format!("bodies/{req_id}.res.bin");
        let path = self.bodies_dir.join(format!("{req_id}.res.bin"));
        write_append(&path, body).await?;
        Ok((body.len(), Some(rel), None))
    }

    /// Open an append-only handle for streaming response chunks. Returns the
    /// path on disk plus the file handle for caller-driven appends.
    pub async fn open_stream_log(&self, req_id: &str) -> Result<(String, tokio::fs::File)> {
        let rel = format!("stream/{req_id}.sse.jsonl");
        let path = self.stream_dir.join(format!("{req_id}.sse.jsonl"));
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .with_context(|| format!("opening {}", path.display()))?;
        Ok((rel, file))
    }

    pub async fn open_ws_log(&self, req_id: &str) -> Result<(String, tokio::fs::File)> {
        let rel = format!("stream/{req_id}.ws.jsonl");
        let path = self.stream_dir.join(format!("{req_id}.ws.jsonl"));
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .with_context(|| format!("opening {}", path.display()))?;
        Ok((rel, file))
    }

    /// Append one SSE event, JSON-encoded, to the per-request stream log.
    pub async fn append_sse_event(file: &mut tokio::fs::File, event: &SseEvent) -> Result<()> {
        let mut line = serde_json::to_vec(event)?;
        line.push(b'\n');
        file.write_all(&line).await?;
        // Match the WS log: flush per event so other readers (Python
        // tools, `tail -f`, the TUI detail view) see SSE events the
        // moment they're parsed, not when the response stream closes.
        file.flush().await?;
        Ok(())
    }

    /// Append a raw response chunk to a per-request `.bin` file. Used for
    /// non-SSE streaming responses where we want the bytes preserved 1:1.
    #[allow(dead_code)]
    pub async fn append_response_chunk(
        &self,
        req_id: &str,
        chunk: &[u8],
    ) -> Result<String> {
        let rel = format!("bodies/{req_id}.res.bin");
        let path = self.bodies_dir.join(format!("{req_id}.res.bin"));
        write_append(&path, chunk).await?;
        Ok(rel)
    }

    pub async fn append_traffic(&self, record: &TrafficRecord) -> Result<()> {
        let mut line = serde_json::to_vec(record)?;
        line.push(b'\n');
        let mut f = self.traffic.lock().await;
        f.write_all(&line).await?;
        Ok(())
    }
}

async fn write_append(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    let mut f = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
        .with_context(|| format!("opening {}", path.display()))?;
    f.write_all(bytes).await?;
    Ok(())
}

/// Find the first UUID-shaped substring (8-4-4-4-12 lowercase hex) inside `s`.
fn find_uuid(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    if bytes.len() < 36 {
        return None;
    }
    let groups = [8usize, 4, 4, 4, 12];
    'outer: for start in 0..=bytes.len().saturating_sub(36) {
        let mut idx = start;
        for (gi, &n) in groups.iter().enumerate() {
            for _ in 0..n {
                if idx >= bytes.len() {
                    continue 'outer;
                }
                let c = bytes[idx];
                let is_hex = c.is_ascii_digit()
                    || (b'a'..=b'f').contains(&c)
                    || (b'A'..=b'F').contains(&c);
                if !is_hex {
                    continue 'outer;
                }
                idx += 1;
            }
            if gi < groups.len() - 1 {
                if idx >= bytes.len() || bytes[idx] != b'-' {
                    continue 'outer;
                }
                idx += 1;
            }
        }
        return Some(s[start..start + 36].to_ascii_lowercase());
    }
    None
}

mod b64 {
    const CHARS: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    pub fn encode(input: &[u8]) -> String {
        let mut out = String::with_capacity(((input.len() + 2) / 3) * 4);
        let mut i = 0;
        while i + 3 <= input.len() {
            let n = ((input[i] as u32) << 16) | ((input[i + 1] as u32) << 8) | (input[i + 2] as u32);
            out.push(CHARS[((n >> 18) & 0x3f) as usize] as char);
            out.push(CHARS[((n >> 12) & 0x3f) as usize] as char);
            out.push(CHARS[((n >> 6) & 0x3f) as usize] as char);
            out.push(CHARS[(n & 0x3f) as usize] as char);
            i += 3;
        }
        let rem = input.len() - i;
        if rem == 1 {
            let n = (input[i] as u32) << 16;
            out.push(CHARS[((n >> 18) & 0x3f) as usize] as char);
            out.push(CHARS[((n >> 12) & 0x3f) as usize] as char);
            out.push('=');
            out.push('=');
        } else if rem == 2 {
            let n = ((input[i] as u32) << 16) | ((input[i + 1] as u32) << 8);
            out.push(CHARS[((n >> 18) & 0x3f) as usize] as char);
            out.push(CHARS[((n >> 12) & 0x3f) as usize] as char);
            out.push(CHARS[((n >> 6) & 0x3f) as usize] as char);
            out.push('=');
        }
        out
    }
}
