//! Compact transcript of a captured session: just the conversation.
//!
//! Raw captures are huge because every agent request re-sends the whole
//! history. Team sync uploads this instead: the person's messages, the agent's
//! replies, one-line tool calls, and folded "context" (hooks, tool lists,
//! AGENTS.md). Same rules as the web app's lib/conversation.ts + lib/codex.ts.
//!
//! - Claude Code: the `/v1/messages` call with the longest history among the
//!   recent ones, plus its streamed reply. Side calls (title generation,
//!   next-prompt suggestions) are skipped or trimmed.
//! - Codex: Responses API over a WebSocket (`stream/*.ws.jsonl`, permessage-
//!   deflate). Each connection is a thread; the one with most user turns wins.

use std::path::Path;

use base64::Engine;
use flate2::{Decompress, FlushDecompress};
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Msg {
    User { at: String, text: String },
    Assistant { at: String, text: String, model: String },
    Tool { at: String, name: String, input: String, #[serde(skip_serializing_if = "Option::is_none")] output: Option<String> },
    Context { at: String, text: String },
}

#[derive(Debug, Serialize)]
pub struct Transcript {
    pub agent: String,
    pub model: String,
    pub turns: usize,
    pub messages: Vec<Msg>,
}

/// Longest single message kept (keeps uploads well under Vercel's 4.5 MB).
const MAX_TEXT: usize = 20_000;

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let mut out: String = s.chars().take(n).collect();
    out.push('…');
    out
}

pub(crate) fn short(v: &Value, n: usize) -> String {
    let s = match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    clip(&s, n)
}

/// Text the harness injected rather than the person typed.
pub(crate) fn is_harness(text: &str) -> bool {
    const P: &[&str] = &[
        "SessionStart:", "UserPromptSubmit hook", "PreToolUse:", "PostToolUse:", "Stop hook", "<local-command",
        "<command-name>", "<command-message>", "Caveat: The messages below", "The following deferred tools",
        "The following skills are available", "The following tools just became available", "[SUGGESTION MODE",
        "<environment_context>", "<user_instructions>", "<permissions", "<INSTRUCTIONS>", "# AGENTS.md",
        "<collaboration_mode>", "<skills_instructions>", "<multi_agent",
    ];
    let t = text.trim_start();
    P.iter().any(|p| t.starts_with(p))
}

pub(crate) fn strip_reminders(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("<system-reminder>") {
        out.push_str(&rest[..start]);
        match rest[start..].find("</system-reminder>") {
            Some(end) => rest = &rest[start + end + "</system-reminder>".len()..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out.trim().to_string()
}

pub(crate) fn push_text(out: &mut Vec<Msg>, role: &str, text: &str, at: &str, model: &str) {
    let text = strip_reminders(text);
    if text.is_empty() {
        return;
    }
    let text = clip(&text, MAX_TEXT);
    let at = at.to_string();
    out.push(if is_harness(&text) {
        Msg::Context { at, text }
    } else if role == "user" {
        Msg::User { at, text }
    } else if role == "assistant" {
        Msg::Assistant { at, text, model: model.to_string() }
    } else {
        Msg::Context { at, text }
    });
}

pub(crate) fn attach_output(out: &mut [Msg], v: &Value) {
    if let Some(Msg::Tool { output, .. }) = out.iter_mut().rev().find(|m| matches!(m, Msg::Tool { .. })) {
        if output.is_none() {
            *output = Some(short(v, 60));
        }
    }
}

// ── Claude Code (Anthropic /v1/messages) ────────────────────────────────────

fn body_text(dir: &Path, side: &Value) -> Option<String> {
    if let Some(inline) = side.get("body_inline").filter(|v| !v.is_null()) {
        let data = inline["data"].as_str()?;
        return match inline["encoding"].as_str() {
            Some("base64") => base64::engine::general_purpose::STANDARD.decode(data).ok().map(|b| String::from_utf8_lossy(&b).into_owned()),
            _ => Some(data.to_string()),
        };
    }
    let p = side["body_path"].as_str()?;
    std::fs::read(dir.join(p)).ok().map(|b| String::from_utf8_lossy(&b).into_owned())
}

fn anthropic_messages(api: &[Value], at: &str, model: &str) -> Vec<Msg> {
    let mut out = vec![];
    for m in api {
        let role = m["role"].as_str().unwrap_or("");
        let blocks: Vec<Value> = match &m["content"] {
            Value::String(s) => vec![serde_json::json!({ "type": "text", "text": s })],
            Value::Array(a) => a.clone(),
            _ => vec![],
        };
        for b in blocks {
            match b["type"].as_str() {
                Some("text") => push_text(&mut out, role, b["text"].as_str().unwrap_or(""), at, model),
                Some("tool_use") => out.push(Msg::Tool {
                    at: at.to_string(),
                    name: b["name"].as_str().unwrap_or("tool").to_string(),
                    input: short(&b["input"], 140),
                    output: None,
                }),
                Some("tool_result") => attach_output(&mut out, &b["content"]),
                _ => {}
            }
        }
    }
    out
}

fn is_side_call(messages: &[Value]) -> bool {
    let Some(last) = messages.last() else { return false };
    if last["role"] != "user" {
        return false;
    }
    let starts = |t: &str| t.trim_start().starts_with("[SUGGESTION MODE");
    match &last["content"] {
        Value::String(s) => starts(s),
        Value::Array(a) => a.iter().any(|b| b["type"] == "text" && b["text"].as_str().is_some_and(starts)),
        _ => false,
    }
}

/// Claude Code's start-up quota check: one user message, "quota". Not a conversation.
fn is_quota_probe(messages: &[Value]) -> bool {
    let [only] = messages else { return false };
    let text = match &only["content"] {
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().filter_map(|b| b["text"].as_str()).collect(),
        _ => return false,
    };
    only["role"] == "user" && text.trim() == "quota"
}

/// True if every model call in a pn run was the quota check (the run was
/// opened and closed without a conversation).
pub fn quota_only(dir: &Path) -> bool {
    let Ok(traffic) = std::fs::read_to_string(dir.join("traffic.jsonl")) else { return false };
    let mut calls = traffic
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|r| r["method"] == "POST" && r["url"].as_str().is_some_and(|u| u.contains("/v1/messages")))
        .peekable();
    calls.peek().is_some()
        && calls.all(|r| {
            body_text(dir, &r["request"])
                .and_then(|b| serde_json::from_str::<Value>(&b).ok())
                .and_then(|req| req["messages"].as_array().map(|m| is_quota_probe(m)))
                .unwrap_or(false)
        })
}

fn reply_from_sse(sse: &str) -> String {
    let mut text = String::new();
    for line in sse.lines() {
        let Ok(ev) = serde_json::from_str::<Value>(line) else { continue };
        let Ok(d) = serde_json::from_str::<Value>(ev["data"].as_str().unwrap_or("")) else { continue };
        if d["type"] == "content_block_delta" && d["delta"]["type"] == "text_delta" {
            text.push_str(d["delta"]["text"].as_str().unwrap_or(""));
        }
    }
    text.trim().to_string()
}

fn claude(dir: &Path) -> Option<Transcript> {
    let traffic = std::fs::read_to_string(dir.join("traffic.jsonl")).ok()?;
    let calls: Vec<Value> = traffic
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|r| r["method"] == "POST" && r["url"].as_str().is_some_and(|u| u.contains("/v1/messages")))
        .collect();
    // (record, side call?, history, model)
    let mut best: Option<(Value, bool, Vec<Value>, String)> = None;
    for r in calls.iter().rev().take(12).rev() {
        let Some(body) = body_text(dir, &r["request"]) else { continue };
        let Ok(req) = serde_json::from_str::<Value>(&body) else { continue };
        let Some(msgs) = req["messages"].as_array() else { continue };
        let side = is_side_call(msgs);
        let history: Vec<Value> = if side { msgs[..msgs.len() - 1].to_vec() } else { msgs.clone() };
        if !history.is_empty() && history.len() >= best.as_ref().map_or(0, |b| b.2.len()) {
            best = Some((r.clone(), side, history, req["model"].as_str().unwrap_or("").to_string()));
        }
    }
    let (r, side, history, model) = best?;
    if is_quota_probe(&history) {
        return None;
    }
    let at = r["ts_start"].as_str().unwrap_or("");
    let mut messages = anthropic_messages(&history, at, &model);
    if !side {
        let reply = match r["response"]["stream_path"].as_str() {
            Some(p) => reply_from_sse(&std::fs::read_to_string(dir.join(p)).unwrap_or_default()),
            None => body_text(dir, &r["response"])
                .and_then(|b| serde_json::from_str::<Value>(&b).ok())
                .and_then(|j| j["content"].as_array().cloned())
                .map(|c| c.iter().filter_map(|b| b["text"].as_str()).collect::<String>().trim().to_string())
                .unwrap_or_default(),
        };
        if !reply.is_empty() {
            push_text(&mut messages, "assistant", &reply, r["ts_end"].as_str().unwrap_or(at), &model);
        }
    }
    let turns = history.iter().filter(|m| m["role"] == "user").count();
    Some(Transcript { agent: "claude".into(), model, turns, messages })
}

// ── Codex (Responses API over a WebSocket) ──────────────────────────────────

/// Decode a `*.ws.jsonl` capture into (ts, direction, text) messages.
fn decode_ws(jsonl: &str) -> Vec<(String, String, String)> {
    const TRAILER: [u8; 4] = [0x00, 0x00, 0xff, 0xff];
    let mut inflaters = std::collections::HashMap::<String, Decompress>::new();
    let mut partial = std::collections::HashMap::<String, (String, Vec<u8>, bool)>::new();
    let mut out = vec![];
    for line in jsonl.lines() {
        let Ok(f) = serde_json::from_str::<Value>(line) else { continue };
        let dir = f["dir"].as_str().unwrap_or("").to_string();
        if dir != "c2s" && dir != "s2c" {
            continue;
        }
        if !matches!(f["op"].as_str(), Some("text" | "binary" | "continuation")) {
            continue;
        }
        let bytes = match f["payload_b64"].as_str() {
            Some(b) => base64::engine::general_purpose::STANDARD.decode(b).unwrap_or_default(),
            None => f["payload"].as_str().unwrap_or("").as_bytes().to_vec(),
        };
        let entry = partial
            .entry(dir.clone())
            .or_insert_with(|| (f["ts"].as_str().unwrap_or("").to_string(), vec![], f["rsv1"].as_bool().unwrap_or(false)));
        entry.1.extend_from_slice(&bytes);
        if !f["fin"].as_bool().unwrap_or(true) {
            continue;
        }
        let (ts, mut data, compressed) = partial.remove(&dir).unwrap_or_default();
        if compressed {
            data.extend_from_slice(&TRAILER);
            let inf = inflaters.entry(dir.clone()).or_insert_with(|| Decompress::new(false));
            let mut plain = Vec::with_capacity(data.len() * 4);
            let mut input = &data[..];
            let mut ok = true;
            loop {
                plain.reserve(64 * 1024);
                let before_in = inf.total_in();
                if inf.decompress_vec(input, &mut plain, FlushDecompress::Sync).is_err() {
                    ok = false;
                    break;
                }
                let used = (inf.total_in() - before_in) as usize;
                input = &input[used..];
                if input.is_empty() && plain.len() < plain.capacity() {
                    break;
                }
                if used == 0 && plain.len() < plain.capacity() {
                    break;
                }
            }
            if !ok {
                continue;
            }
            data = plain;
        }
        out.push((ts, dir, String::from_utf8_lossy(&data).into_owned()));
    }
    out
}

pub(crate) fn codex_item(out: &mut Vec<Msg>, it: &Value, at: &str, model: &str) {
    match it["type"].as_str() {
        Some("message") => {
            let text = match &it["content"] {
                Value::String(s) => s.clone(),
                Value::Array(parts) => parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n"),
                _ => String::new(),
            };
            let role = match it["role"].as_str() {
                Some("user") => "user",
                Some("assistant") => "assistant",
                _ => "context", // developer / system instructions
            };
            push_text(out, role, &text, at, model);
        }
        Some("function_call" | "custom_tool_call") => out.push(Msg::Tool {
            at: at.to_string(),
            name: it["name"].as_str().unwrap_or("tool").to_string(),
            input: short(if it["arguments"].is_null() { &it["input"] } else { &it["arguments"] }, 140),
            output: None,
        }),
        Some("local_shell_call") => out.push(Msg::Tool {
            at: at.to_string(),
            name: "shell".into(),
            input: clip(
                &it["action"]["command"].as_array().map(|c| c.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(" ")).unwrap_or_default(),
                140,
            ),
            output: None,
        }),
        Some("function_call_output" | "custom_tool_call_output") => attach_output(out, &it["output"]),
        _ => {}
    }
}

fn codex_thread(log: &str) -> (Vec<Msg>, usize, String) {
    let mut out = vec![];
    let mut model = String::new();
    for (ts, dir, text) in decode_ws(log) {
        let Ok(ev) = serde_json::from_str::<Value>(&text) else { continue };
        if dir == "c2s" && ev["type"] == "response.create" {
            if ev["generate"] == false {
                continue; // connection warm-up
            }
            if let Some(m) = ev["model"].as_str() {
                model = m.to_string();
            }
            if ev["previous_response_id"].is_null() {
                out.clear(); // a fresh request restates the whole history
            }
            for it in ev["input"].as_array().into_iter().flatten() {
                codex_item(&mut out, it, &ts, &model);
            }
        } else if dir == "s2c" && ev["type"] == "response.output_item.done" {
            codex_item(&mut out, &ev["item"], &ts, &model);
        }
    }
    let turns = out.iter().filter(|m| matches!(m, Msg::User { .. })).count();
    (out, turns, model)
}

fn codex(dir: &Path) -> Option<Transcript> {
    let mut logs: Vec<_> = std::fs::read_dir(dir.join("stream"))
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.to_string_lossy().ends_with(".ws.jsonl"))
        .collect();
    logs.sort();
    let mut best: Option<(Vec<Msg>, usize, String)> = None;
    for p in logs {
        let t = codex_thread(&std::fs::read_to_string(&p).unwrap_or_default());
        // Ties keep the earlier thread: the main one opens before side ones (titles).
        if t.1 > best.as_ref().map_or(0, |b| b.1) {
            best = Some(t);
        }
    }
    let (messages, turns, model) = best?;
    Some(Transcript { agent: "codex".into(), model, turns, messages })
}

/// Build the transcript for a session directory (None if nothing conversational yet).
pub fn build(dir: &Path) -> Option<Transcript> {
    claude(dir).filter(|t| !t.messages.is_empty()).or_else(|| codex(dir))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_reminders_and_marks_harness() {
        assert_eq!(strip_reminders("hi <system-reminder>x</system-reminder> there"), "hi  there");
        assert!(is_harness("SessionStart:startup hook success"));
        assert!(!is_harness("What is Claude Shannon"));
    }

    #[test]
    fn quota_probe_is_not_a_conversation() {
        assert!(is_quota_probe(&[serde_json::json!({"role": "user", "content": "quota"})]));
        assert!(is_quota_probe(&[serde_json::json!({"role": "user", "content": [{"type": "text", "text": "quota"}]})]));
        assert!(!is_quota_probe(&[serde_json::json!({"role": "user", "content": "quota limits?"})]));
        assert!(!is_quota_probe(&[serde_json::json!({"role": "user", "content": "quota"}), serde_json::json!({"role": "assistant", "content": "ok"})]));
    }

    #[test]
    fn side_call_detection() {
        let msgs = vec![serde_json::json!({"role":"user","content":[{"type":"text","text":"[SUGGESTION MODE: x]"}]})];
        assert!(is_side_call(&msgs));
    }
}
