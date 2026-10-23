//! Sessions Claude Code and Codex save themselves, whether or not they ran
//! through Pine: plain `claude` / `codex`, and both desktop apps.
//!
//! - Claude Code: `~/.claude/projects/<folder>/<uuid>.jsonl`, one line per
//!   message block (text, tool_use, tool_result, thinking) plus bookkeeping.
//! - Codex: `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`, Responses API items.
//!
//! Both are append-only, so file size is the change signal. Each one maps to a
//! fixed Pine id: the native UUID's 128 bits read as a ULID.

use std::collections::HashSet;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde_json::Value;

use crate::transcript::{attach_output, codex_item, push_text, short, Msg, Transcript};

pub struct Native {
    /// Pine session id (from the native UUID).
    pub id: String,
    pub native_id: String,
    pub agent: &'static str,
    pub path: PathBuf,
    pub size: u64,
    /// When the file was created (ms since epoch): the "since login" check.
    pub created_ms: u64,
}

/// What registering needs, read from inside the file.
pub struct Meta {
    pub cwd: String,
    pub started_at: String,
    pub title: Option<String>,
}

/// "8afb629b-22c1-…" → the same 128 bits as a ULID.
fn ulid_of(uuid: &str) -> Option<String> {
    let hex: String = uuid.chars().filter(|c| *c != '-').collect();
    (hex.len() == 32).then(|| u128::from_str_radix(&hex, 16).ok()).flatten().map(|n| ulid::Ulid(n).to_string())
}

fn entry(path: PathBuf, native_id: &str, agent: &'static str) -> Option<Native> {
    let m = std::fs::metadata(&path).ok()?;
    let created = m.created().or_else(|_| m.modified()).ok()?;
    Some(Native {
        id: ulid_of(native_id)?,
        native_id: native_id.to_string(),
        agent,
        size: m.len(),
        created_ms: created.duration_since(UNIX_EPOCH).ok()?.as_millis() as u64,
        path,
    })
}

fn home() -> Option<PathBuf> {
    dirs::home_dir()
}

/// Every Claude Code and Codex session on this machine (just a directory walk and stat).
pub fn discover() -> Vec<Native> {
    let mut out = vec![];
    let Some(home) = home() else { return out };
    // Claude: top-level <uuid>.jsonl in each project folder (sub-agent files live deeper).
    for proj in std::fs::read_dir(home.join(".claude/projects")).into_iter().flatten().flatten() {
        for f in std::fs::read_dir(proj.path()).into_iter().flatten().flatten() {
            let p = f.path();
            let Some(stem) = p.file_stem().map(|s| s.to_string_lossy().to_string()) else { continue };
            if p.extension().is_some_and(|x| x == "jsonl") {
                out.extend(entry(p, &stem, "claude"));
            }
        }
    }
    // Codex: rollout-<date>-<uuid>.jsonl; the uuid is the last 36 chars of the stem.
    fn walk(d: &Path, out: &mut Vec<Native>) {
        for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
                continue;
            }
            let stem = p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            if stem.starts_with("rollout-") && stem.len() > 36 && p.extension().is_some_and(|x| x == "jsonl") {
                let id = stem[stem.len() - 36..].to_string();
                out.extend(entry(p, &id, "codex"));
            }
        }
    }
    walk(&home.join(".codex/sessions"), &mut out);
    out.sort_by_key(|n| n.created_ms);
    out
}

/// Codex thread names (`session_index.jsonl`).
fn codex_title(id: &str) -> Option<String> {
    let text = std::fs::read_to_string(home()?.join(".codex/session_index.jsonl")).ok()?;
    text.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["id"] == id)
        .filter_map(|v| v["thread_name"].as_str().map(str::to_string))
        .last()
}

fn text_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

/// A Claude Code session file → transcript. Sidechains (sub-agents) and
/// meta lines are skipped; thinking isn't shared.
fn claude(path: &Path) -> Option<(Transcript, Meta)> {
    let f = std::fs::File::open(path).ok()?;
    let (mut out, mut model, mut cwd, mut started, mut title) = (vec![], String::new(), String::new(), String::new(), None);
    let mut real_reply = false; // any reply Claude wrote, not one copied in by pn handoff / pull
    for line in BufReader::new(f).lines().map_while(Result::ok) {
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        match v["type"].as_str() {
            Some("ai-title") => title = v["aiTitle"].as_str().map(str::to_string),
            Some("summary") if title.is_none() => title = v["summary"].as_str().map(str::to_string),
            Some(role @ ("user" | "assistant")) => {
                if v["isSidechain"] == true || v["isMeta"] == true {
                    continue;
                }
                let at = v["timestamp"].as_str().unwrap_or_default();
                if cwd.is_empty() {
                    cwd = v["cwd"].as_str().unwrap_or_default().to_string();
                }
                if started.is_empty() {
                    started = at.to_string();
                }
                let msg = &v["message"];
                if role == "assistant" && !msg["id"].as_str().unwrap_or_default().starts_with("msg_pine_") {
                    real_reply = true;
                }
                if let Some(m) = msg["model"].as_str().filter(|m| !m.starts_with('<')) {
                    model = m.to_string();
                }
                match &msg["content"] {
                    Value::String(s) => push_text(&mut out, role, s, at, &model),
                    Value::Array(blocks) => {
                        for b in blocks {
                            match b["type"].as_str() {
                                Some("text") => push_text(&mut out, role, b["text"].as_str().unwrap_or_default(), at, &model),
                                Some("tool_use") => out.push(Msg::Tool {
                                    at: at.to_string(),
                                    name: b["name"].as_str().unwrap_or("tool").to_string(),
                                    input: short(&b["input"], 140),
                                    output: None,
                                }),
                                Some("tool_result") => attach_output(&mut out, &Value::String(text_of(&b["content"]))),
                                _ => {} // thinking, images
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    if !real_reply {
        return None; // only a copy so far; it syncs once Claude answers in it
    }
    let turns = out.iter().filter(|m| matches!(m, Msg::User { .. })).count();
    Some((Transcript { agent: "claude".into(), model, turns, messages: out }, Meta { cwd, started_at: started, title }))
}

/// A Codex rollout → transcript. Follows Codex's own compaction. None for
/// sub-agent threads (they show up inside their parent).
fn codex(path: &Path, id: &str) -> Option<(Transcript, Meta)> {
    let f = std::fs::File::open(path).ok()?;
    let (mut out, mut model, mut cwd, mut started) = (vec![], String::new(), String::new(), String::new());
    for line in BufReader::new(f).lines().map_while(Result::ok) {
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        let (p, at) = (&v["payload"], v["timestamp"].as_str().unwrap_or_default());
        match v["type"].as_str() {
            Some("session_meta") => {
                if p["thread_source"] == "subagent" || p["source"].get("subagent").is_some() {
                    return None;
                }
                cwd = p["cwd"].as_str().unwrap_or_default().to_string();
                started = p["timestamp"].as_str().unwrap_or(at).to_string();
            }
            Some("turn_context") => {
                if let Some(m) = p["model"].as_str() {
                    model = m.to_string();
                }
            }
            Some("response_item") => codex_item(&mut out, p, at, &model),
            Some("compacted") => {
                out.clear();
                for it in p["replacement_history"].as_array().into_iter().flatten() {
                    codex_item(&mut out, it, at, &model);
                }
            }
            _ => {}
        }
    }
    let turns = out.iter().filter(|m| matches!(m, Msg::User { .. })).count();
    Some((Transcript { agent: "codex".into(), model, turns, messages: out }, Meta { cwd, started_at: started, title: codex_title(id) }))
}

/// Read one native session. None: a Codex sub-agent thread (never shared), or
/// a Claude session that is still only a copy made by `pn handoff` / `pn pull`.
pub fn read(n: &Native) -> Option<(Transcript, Meta)> {
    match n.agent {
        "claude" => claude(&n.path),
        _ => codex(&n.path, &n.native_id),
    }
}

/// Native sessions that are really `pn` / `pn codex` runs, already synced from
/// `~/.pine/sessions`: Claude by the session id pn saw, Codex by folder and
/// start time (within 30 s).
pub struct Recorded {
    claude_ids: HashSet<String>,
    codex: Vec<(String, u64)>,
}

impl Recorded {
    pub fn from_pine(sessions: &[(String, PathBuf)]) -> Self {
        let mut r = Recorded { claude_ids: HashSet::new(), codex: vec![] };
        for (id, dir) in sessions {
            let Some(meta) = std::fs::read(dir.join("meta.json")).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()) else { continue };
            if let Some(c) = meta["claude_session_id"].as_str() {
                r.claude_ids.insert(c.to_string());
            }
            if meta["target"] == "codex" {
                let ms = id.parse::<ulid::Ulid>().map(|u| u.timestamp_ms()).unwrap_or(0);
                r.codex.push((meta["cwd"].as_str().unwrap_or_default().to_string(), ms));
            }
        }
        r
    }

    pub fn covers(&self, n: &Native, meta: &Meta) -> bool {
        match n.agent {
            "claude" => self.claude_ids.contains(&n.native_id),
            _ => self.codex.iter().any(|(cwd, ms)| *cwd == meta.cwd && ms.abs_diff(n.created_ms) < 30_000),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_maps_to_a_stable_ulid() {
        let a = ulid_of("8afb629b-22c1-4659-be4c-daa352fa01e2").unwrap();
        assert_eq!(a.len(), 26);
        assert_eq!(Some(a.clone()), ulid_of("8afb629b-22c1-4659-be4c-daa352fa01e2"));
        assert_eq!(ulid::Ulid::from_string(&a).unwrap().0, 0x8afb629b22c14659be4cdaa352fa01e2);
        assert!(ulid_of("not-a-uuid").is_none());
    }

    #[test]
    fn reads_claude_file() {
        let dir = std::env::temp_dir().join(format!("pine-native-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("x.jsonl");
        let lines = [
            r#"{"type":"ai-title","aiTitle":"Fix login"}"#,
            r#"{"type":"user","cwd":"/r","timestamp":"2026-10-01T00:00:00Z","message":{"role":"user","content":"fix the login bug"}}"#,
            r#"{"type":"assistant","timestamp":"t","message":{"model":"claude-x","content":[{"type":"thinking","thinking":"hmm"}]}}"#,
            r#"{"type":"assistant","timestamp":"t","message":{"model":"claude-x","content":[{"type":"tool_use","name":"Bash","input":{"command":"ls"}}]}}"#,
            r#"{"type":"user","timestamp":"t","message":{"content":[{"type":"tool_result","content":"a.rs"}]}}"#,
            r#"{"type":"assistant","isSidechain":true,"timestamp":"t","message":{"content":[{"type":"text","text":"sub-agent"}]}}"#,
            r#"{"type":"assistant","timestamp":"t","message":{"model":"claude-x","content":[{"type":"text","text":"Fixed."}]}}"#,
        ];
        std::fs::write(&f, lines.join("\n")).unwrap();
        let (t, m) = claude(&f).unwrap();
        assert_eq!((m.cwd.as_str(), m.title.as_deref(), t.model.as_str(), t.turns), ("/r", Some("Fix login"), "claude-x", 1));
        assert_eq!(t.messages.len(), 3);
        assert!(matches!(&t.messages[1], Msg::Tool { name, output: Some(o), .. } if name == "Bash" && o == "a.rs"));
        std::fs::remove_dir_all(dir).ok();
    }
}

#[cfg(test)]
mod real {
    /// Reads your own Claude Code / Codex files, uploads nothing:
    /// `cargo test real_native -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn real_native() {
        let all = super::discover();
        let claude = all.iter().filter(|n| n.agent == "claude").count();
        println!("found {} files: {claude} claude, {} codex", all.len(), all.len() - claude);
        let t0 = std::time::Instant::now();
        let mut skipped = 0;
        for n in all.iter().rev().take(6) {
            match super::read(n) {
                Some((t, m)) => println!("{} {} {} msgs, {} turns | {} | {:?}", n.agent, &n.id[..8], t.messages.len(), t.turns, m.cwd, m.title),
                None => skipped += 1,
            }
        }
        println!("read 6 newest in {:?}, {skipped} sub-agent threads skipped", t0.elapsed());
    }
}
