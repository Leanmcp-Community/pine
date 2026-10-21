//! Continue a pulled conversation in your own agent.
//!
//! - Claude Code: write the transcript as a native session file under
//!   `~/.claude/projects/<cwd>/<uuid>.jsonl`, so `pn -- --resume <uuid>` (or
//!   `claude --resume <uuid>`) picks it up in this folder.
//! - Codex: its session format isn't stable enough to fake, so write the
//!   conversation as Markdown and start a fresh Codex session that reads it.
//!
//! Tool calls survive only as one-line text summaries (that's all a transcript
//! keeps), so the agent sees what was done, not the full tool output.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{json, Value};

const MAX_TOOL_OUTPUT: usize = 2_000;

fn s<'a>(m: &'a Value, k: &str) -> &'a str {
    m[k].as_str().unwrap_or("")
}

fn clip(t: &str, n: usize) -> String {
    match t.char_indices().nth(n) {
        Some((i, _)) => format!("{}…", &t[..i]),
        None => t.to_string(),
    }
}

/// Alternating (role, text) turns. Context notes are dropped; tool calls fold
/// into the assistant turn that made them; consecutive same-role messages merge.
fn turns(t: &Value, label: &str) -> Vec<(&'static str, String, String)> {
    let mut out: Vec<(&'static str, String, String)> = Vec::new();
    for m in t["messages"].as_array().into_iter().flatten() {
        let (role, text) = match s(m, "kind") {
            "user" => ("user", s(m, "text").to_string()),
            "assistant" => ("assistant", s(m, "text").to_string()),
            "tool" => {
                let mut line = format!("[tool: {}] {}", s(m, "name"), s(m, "input"));
                if !s(m, "output").is_empty() {
                    line += &format!("\n→ {}", clip(s(m, "output"), MAX_TOOL_OUTPUT));
                }
                ("assistant", line)
            }
            _ => continue,
        };
        if text.trim().is_empty() {
            continue;
        }
        match out.last_mut() {
            Some(last) if last.0 == role => {
                last.1.push_str("\n\n");
                last.1.push_str(&text);
            }
            _ => out.push((role, text, s(m, "at").to_string())),
        }
    }
    let note = format!("[Conversation pulled from Pine ({label}). Tool calls appear as one-line summaries.]");
    match out.first_mut() {
        Some(first) if first.0 == "user" => first.1 = format!("{note}\n\n{}", first.1),
        _ => out.insert(0, ("user", note, String::new())),
    }
    out
}

/// A random v4 UUID (Claude Code names sessions by UUID).
fn uuid4() -> String {
    let mut b = ulid::Ulid::new().0.to_be_bytes();
    // A ULID's first 6 bytes are a timestamp; mix in more randomness.
    let r = ulid::Ulid::new().0.to_le_bytes();
    for i in 0..6 {
        b[i] ^= r[i];
    }
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

/// Claude Code keys projects by the absolute cwd with every non-alphanumeric char as `-`.
fn claude_project_dir(cwd: &Path) -> Result<PathBuf> {
    let key: String = cwd.to_string_lossy().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    Ok(dirs::home_dir().context("no home dir")?.join(".claude/projects").join(key))
}

/// Write the conversation as a Claude Code session for `cwd`; returns its id.
pub fn into_claude(t: &Value, label: &str, cwd: &Path) -> Result<String> {
    let sid = uuid4();
    let cwd_s = cwd.to_string_lossy().to_string();
    let model = s(t, "model");
    let now = crate::team::now_rfc3339();
    let title = clip(&format!("Pulled: {label}"), 80);
    let mut lines = vec![json!({ "type": "ai-title", "aiTitle": title, "sessionId": sid })];
    let mut parent: Option<String> = None;
    for (i, (role, text, at)) in turns(t, label).into_iter().enumerate() {
        let uuid = uuid4();
        let message = if role == "user" {
            json!({ "role": "user", "content": text })
        } else {
            json!({
                "id": format!("msg_pine_{i}"), "type": "message", "role": "assistant",
                "model": if model.is_empty() { "unknown" } else { model },
                "content": [{ "type": "text", "text": text }],
                "stop_reason": "end_turn", "stop_sequence": null,
                "usage": { "input_tokens": 0, "output_tokens": 0 },
            })
        };
        lines.push(json!({
            "parentUuid": parent, "isSidechain": false, "type": role, "message": message,
            "uuid": uuid, "timestamp": if at.is_empty() { now.as_str() } else { at.as_str() },
            "userType": "external", "entrypoint": "cli", "cwd": cwd_s, "sessionId": sid, "gitBranch": "",
        }));
        parent = Some(uuid);
    }
    let dir = claude_project_dir(cwd)?;
    std::fs::create_dir_all(&dir)?;
    let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
    std::fs::write(dir.join(format!("{sid}.jsonl")), body)?;
    Ok(sid)
}

/// The conversation as Markdown, for agents we can't resume natively.
pub fn markdown(t: &Value, label: &str, out: &Path) -> Result<()> {
    let mut md = format!("# Conversation pulled from Pine: {label}\n\n");
    for (i, (role, text, _)) in turns(t, label).into_iter().enumerate() {
        // The heading already says where it came from; drop the resume note.
        let text = match (i, text.split_once("\n\n")) {
            (0, Some((note, rest))) if note.starts_with("[Conversation pulled") => rest.to_string(),
            (0, None) if text.starts_with("[Conversation pulled") => continue,
            _ => text,
        };
        md += &format!("## {}\n\n{}\n\n", if role == "user" { "User" } else { "Agent" }, text);
    }
    std::fs::write(out, md)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turns_alternate_and_fold_tools() {
        let t = json!({ "messages": [
            { "kind": "context", "at": "", "text": "<system-reminder>" },
            { "kind": "user", "at": "t1", "text": "fix the bug" },
            { "kind": "assistant", "at": "t2", "text": "Looking." },
            { "kind": "tool", "at": "t3", "name": "Bash", "input": "ls", "output": "a.rs" },
            { "kind": "assistant", "at": "t4", "text": "Fixed." },
            { "kind": "user", "at": "t5", "text": "thanks" },
        ]});
        let v = turns(&t, "checkpoint \"x\"");
        let roles: Vec<_> = v.iter().map(|x| x.0).collect();
        assert_eq!(roles, ["user", "assistant", "user"]);
        assert!(v[0].1.starts_with("[Conversation pulled from Pine (checkpoint \"x\")"));
        assert!(v[0].1.ends_with("fix the bug"));
        assert_eq!(v[1].1, "Looking.\n\n[tool: Bash] ls\n→ a.rs\n\nFixed.");
    }

    #[test]
    fn claude_project_key_and_uuid() {
        let d = claude_project_dir(Path::new("/Users/ddod/LEANMCP/PINE_MD")).unwrap();
        assert!(d.ends_with(".claude/projects/-Users-ddod-LEANMCP-PINE-MD"));
        let u = uuid4();
        assert_eq!(u.len(), 36);
        assert_eq!(&u[14..15], "4");
    }
}
