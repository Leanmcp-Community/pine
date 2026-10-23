//! Write a conversation as a native Codex session, so `codex resume <id>`
//! (and the Codex desktop app) continue it.
//!
//! Codex's format changes between versions, so we don't invent it: we copy
//! the newest real thread on this machine as a template.
//!
//! - Rollout file: its `session_meta` (system instructions, version, provider)
//!   and `turn_context` (model, sandbox, approvals), with our id, folder and
//!   git; then per turn the same lines Codex writes itself: task_started,
//!   turn_context, the user / assistant `response_item`s (what the model gets
//!   back) and `item_completed` events (what the UI shows), task_complete.
//! - `state_5.sqlite` `threads` row: the resume list and the app read this.
//!   We copy the template's row and change the id, path, folder and title.
//! - `session_index.jsonl`: the thread's name.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::resume::{clip, Turn};

fn codex_home() -> Result<PathBuf> {
    Ok(dirs::home_dir().context("no home dir")?.join(".codex"))
}

/// A time-ordered v7 UUID, like Codex's own thread and turn ids.
pub fn uuid7() -> String {
    let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
    let r = ulid::Ulid::new().0.to_le_bytes(); // low 80 bits of a ULID are random
    let mut b = [0u8; 16];
    b[..6].copy_from_slice(&ms.to_be_bytes()[2..]);
    b[6] = 0x70 | (r[0] & 0x0f);
    b[7] = r[1];
    b[8] = 0x80 | (r[2] & 0x3f);
    b[9..].copy_from_slice(&r[3..10]);
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

fn now() -> (time::OffsetDateTime, String) {
    let t = time::OffsetDateTime::now_utc();
    let s = t.format(&time::macros::format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z")).unwrap_or_default();
    (t, s)
}

/// The newest top-level Codex thread and its first session_meta / turn_context lines.
fn template() -> Result<(String, Value, Value)> {
    for t in crate::handoff::codex_threads()? {
        let Ok(text) = std::fs::read_to_string(&t.path) else { continue };
        let mut meta = None;
        for v in text.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()) {
            match v["type"].as_str() {
                Some("session_meta") if meta.is_none() => meta = Some(v["payload"].clone()),
                Some("turn_context") => {
                    if let Some(m) = meta {
                        return Ok((t.id, m, v["payload"].clone()));
                    }
                }
                _ => {}
            }
        }
    }
    bail!("Use Codex once on this machine first (any folder), so Pine can copy its session format")
}

fn sql(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Write `turns` as a Codex thread for `cwd`; returns its id.
pub fn write_codex(turns: Vec<Turn>, title: &str, cwd: &Path) -> Result<String> {
    let (template_id, mut meta, ctx) = template()?;
    let id = uuid7();
    let (t0, ts) = now();
    let secs = t0.unix_timestamp();
    let ms = (t0.unix_timestamp_nanos() / 1_000_000) as i64;
    let cwd_s = cwd.to_string_lossy().to_string();
    let date = ts.get(..10).unwrap_or_default().to_string();
    let git = |args: &[&str]| crate::team::git(&cwd_s, args);
    let (sha, branch, origin) = (git(&["rev-parse", "HEAD"]), git(&["rev-parse", "--abbrev-ref", "HEAD"]), git(&["remote", "get-url", "origin"]));

    // session_meta: the template's, made ours.
    let m = meta.as_object_mut().context("odd Codex session_meta")?;
    for k in ["forked_from_id", "parent_thread_id", "agent_nickname", "agent_path", "agent_role", "subagent_history_start_ordinal"] {
        m.remove(k);
    }
    m.insert("session_id".into(), json!(id));
    m.insert("id".into(), json!(id));
    m.insert("timestamp".into(), json!(ts));
    m.insert("cwd".into(), json!(cwd_s));
    m.insert("runtime_workspace_roots".into(), json!([cwd_s]));
    m.insert("source".into(), json!("cli"));
    m.insert("thread_source".into(), json!("user"));
    m.insert("originator".into(), json!("codex-tui"));
    if m.contains_key("context_window") {
        m.insert("context_window".into(), json!({ "window_id": uuid7() }));
    }
    match &sha {
        Some(sha) => m.insert("git".into(), json!({ "commit_hash": sha, "branch": branch, "repository_url": origin })),
        None => m.remove("git"),
    };

    let mut lines: Vec<Value> = vec![];
    let mut push = |typ: &str, payload: Value| {
        let ordinal = lines.len();
        lines.push(json!({ "timestamp": ts, "ordinal": ordinal, "type": typ, "payload": payload }));
    };
    push("session_meta", meta);

    let first_user = turns
        .iter()
        .find(|t| t.0 == "user")
        .map(|t| t.1.split_once("\n\n").map(|(_, rest)| rest).unwrap_or(&t.1).to_string()) // skip the handoff note
        .unwrap_or_default();
    let mut iter = turns.into_iter().peekable();
    while let Some((role, text, _)) = iter.next() {
        if role != "user" {
            continue; // alternate() always starts with a user turn; stray replies have nowhere to go
        }
        let turn = uuid7();
        push("event_msg", json!({ "type": "task_started", "turn_id": turn, "started_at": secs, "collaboration_mode_kind": "default" }));
        let mut c = ctx.clone();
        if let Some(o) = c.as_object_mut() {
            o.insert("turn_id".into(), json!(turn));
            o.insert("root_turn_id".into(), json!(turn));
            o.insert("cwd".into(), json!(cwd_s));
            o.insert("workspace_roots".into(), json!([cwd_s]));
            o.insert("current_date".into(), json!(date));
            o.remove("user_instructions"); // the template folder's AGENTS.md; Codex re-reads this folder's
        }
        push("turn_context", c);
        push("response_item", json!({ "type": "message", "id": format!("msg_{}", uuid7()), "role": "user", "content": [{ "type": "input_text", "text": text }] }));
        push("event_msg", json!({ "type": "item_completed", "thread_id": id, "turn_id": turn,
            "item": { "type": "UserMessage", "id": uuid7(), "content": [{ "type": "text", "text": text, "text_elements": [] }] },
            "started_at_ms": ms, "completed_at_ms": ms }));
        let reply = match iter.peek() {
            Some((r, _, _)) if *r == "assistant" => iter.next().map(|t| t.1).unwrap_or_default(),
            _ => String::new(),
        };
        if !reply.is_empty() {
            let mid = format!("msg_{}", uuid7());
            push("response_item", json!({ "type": "message", "id": mid, "role": "assistant", "content": [{ "type": "output_text", "text": reply }], "phase": "final_answer" }));
            push("event_msg", json!({ "type": "item_completed", "thread_id": id, "turn_id": turn,
                "item": { "type": "AgentMessage", "id": mid, "content": [{ "type": "Text", "text": reply }], "phase": "final_answer" },
                "started_at_ms": ms, "completed_at_ms": ms }));
        }
        push("event_msg", json!({ "type": "task_complete", "turn_id": turn, "last_agent_message": clip(&reply, 500), "started_at": secs, "completed_at": secs, "duration_ms": 0 }));
    }

    // ~/.codex/sessions/YYYY/MM/DD/rollout-<time>-<id>.jsonl
    let stamp = t0.format(&time::macros::format_description!("[year]-[month]-[day]T[hour]-[minute]-[second]")).unwrap_or_default();
    let dir = codex_home()?.join("sessions").join(&stamp[0..4]).join(&stamp[5..7]).join(&stamp[8..10]);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("rollout-{stamp}-{id}.jsonl"));
    std::fs::write(&path, lines.iter().map(|l| format!("{l}\n")).collect::<String>())?;

    // The thread's name, as Codex records it.
    let mut idx = std::fs::OpenOptions::new().create(true).append(true).open(codex_home()?.join("session_index.jsonl"))?;
    writeln!(idx, "{}", json!({ "id": id, "thread_name": title, "updated_at": ts }))?;

    // The resume list / desktop app: a copy of the template's row, made ours.
    let db = codex_home()?.join("state_5.sqlite");
    if db.exists() {
        let preview = clip(first_user.trim(), 200);
        let script = format!(
            "BEGIN;
CREATE TEMP TABLE pine_t AS SELECT * FROM threads WHERE id = {tid};
UPDATE pine_t SET id = {id}, rollout_path = {path}, created_at = {secs}, updated_at = {secs}, created_at_ms = {ms}, updated_at_ms = {ms},
  recency_at = {secs}, recency_at_ms = {ms}, cwd = {cwd}, title = {preview}, first_user_message = {preview}, preview = {preview}, name = {name},
  tokens_used = 0, archived = 0, archived_at = NULL, is_pinned = 0, thread_section_id = NULL, section_position = NULL,
  section_entered_at_ms = NULL, project_id = NULL, source = 'cli', thread_source = 'user', originator = 'codex-tui',
  agent_nickname = NULL, agent_role = NULL, agent_path = NULL,
  git_sha = {sha}, git_branch = {branch}, git_origin_url = {origin};
INSERT INTO threads SELECT * FROM pine_t;
COMMIT;
",
            tid = sql(&template_id),
            id = sql(&id),
            path = sql(&path.to_string_lossy()),
            cwd = sql(&cwd_s),
            preview = sql(&preview),
            name = sql(title),
            sha = sha.as_deref().map(sql).unwrap_or("NULL".into()),
            branch = branch.as_deref().map(sql).unwrap_or("NULL".into()),
            origin = origin.as_deref().map(sql).unwrap_or("NULL".into()),
        );
        let bin = if Path::new("/usr/bin/sqlite3").exists() { "/usr/bin/sqlite3" } else { "sqlite3" };
        let mut child = Command::new(bin).arg(&db).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn().context("sqlite3 not found")?;
        child.stdin.take().unwrap().write_all(script.as_bytes())?;
        let out = child.wait_with_output()?;
        if !out.status.success() {
            // The file alone still works with `codex resume <id>`; only the list misses it.
            eprintln!("note: couldn't add it to Codex's session list ({}).", String::from_utf8_lossy(&out.stderr).trim());
        }
    }
    Ok(id)
}

#[cfg(test)]
mod tests {
    #[test]
    fn uuid7_shape() {
        let a = super::uuid7();
        assert_eq!(a.len(), 36);
        assert_eq!(&a[14..15], "7");
        assert!(matches!(&a[19..20], "8" | "9" | "a" | "b"));
        assert_ne!(a, super::uuid7());
    }
}

#[cfg(test)]
mod roundtrip {
    use std::path::Path;

    /// Fake ~/.codex with one template thread → write → read back with our own Codex reader.
    /// Runs alone (it points HOME at a temp dir): `cargo test codex_roundtrip -- --ignored`.
    #[test]
    #[ignore]
    fn codex_roundtrip() {
        let home = std::env::temp_dir().join(format!("pine-home-{}", ulid::Ulid::new()));
        let day = home.join(".codex/sessions/2026/10/01");
        std::fs::create_dir_all(&day).unwrap();
        let tid = "01a00000-0000-7000-8000-000000000001";
        std::fs::write(
            day.join(format!("rollout-2026-10-01T00-00-00-{tid}.jsonl")),
            format!(
                "{}\n{}\n",
                serde_json::json!({"timestamp":"t","ordinal":0,"type":"session_meta","payload":{"id":tid,"session_id":tid,"cwd":"/elsewhere","base_instructions":{"text":"You are Codex"},"cli_version":"0.155.1","source":"cli","thread_source":"user"}}),
                serde_json::json!({"timestamp":"t","ordinal":1,"type":"turn_context","payload":{"model":"gpt-x","cwd":"/elsewhere","user_instructions":"other AGENTS.md"}})
            ),
        )
        .unwrap();
        let db = home.join(".codex/state_5.sqlite");
        let ok = std::process::Command::new("/usr/bin/sqlite3")
            .arg(&db)
            .arg(format!("CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, created_at INT, updated_at INT, created_at_ms INT, updated_at_ms INT, recency_at INT, recency_at_ms INT, source TEXT, cwd TEXT, title TEXT, first_user_message TEXT, preview TEXT, name TEXT, tokens_used INT, archived INT, archived_at INT, is_pinned INT, thread_section_id TEXT, section_position INT, section_entered_at_ms INT, project_id TEXT, thread_source TEXT, originator TEXT, agent_nickname TEXT, agent_role TEXT, agent_path TEXT, git_sha TEXT, git_branch TEXT, git_origin_url TEXT, model TEXT); INSERT INTO threads (id, model) VALUES ('{tid}', 'gpt-x');"))
            .status()
            .unwrap()
            .success();
        assert!(ok);
        std::env::set_var("HOME", &home);

        let turns = vec![
            ("user", "[note]\n\nbuild the page".to_string(), String::new()),
            ("assistant", "[tool: Bash] ls\n→ a.rs\n\nDone.".to_string(), String::new()),
            ("user", "thanks".to_string(), String::new()),
        ];
        let repo = home.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let id = super::write_codex(turns, "From Claude: Page", &repo).unwrap();

        let t = crate::handoff::codex_threads().unwrap().into_iter().find(|t| t.id == id).expect("listed like a real thread");
        assert_eq!(t.cwd, repo.to_string_lossy());
        assert_eq!(t.title, "From Claude: Page");
        let back = crate::handoff::read_codex(&t.path, &t.title).unwrap();
        let texts: Vec<_> = back.turns.iter().map(|x| (x.0, x.1.lines().last().unwrap_or_default().to_string())).collect();
        assert_eq!(texts.last().unwrap(), &("user", "thanks".to_string()));
        assert!(back.turns.iter().any(|x| x.1.contains("Done.")));
        let raw = std::fs::read_to_string(&t.path).unwrap();
        assert!(!raw.contains("other AGENTS.md"));
        assert!(raw.contains("You are Codex"));
        let row = std::process::Command::new("/usr/bin/sqlite3").arg(&db).arg(format!("select cwd || '|' || name || '|' || preview || '|' || model from threads where id = '{id}'")).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&row.stdout).trim(), format!("{}|From Claude: Page|build the page|gpt-x", repo.display()));
        let _ = Path::new(&home);
        std::fs::remove_dir_all(home).ok();
    }
}
