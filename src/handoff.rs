//! Move a conversation between agents on this machine: `pn handoff claude`
//! turns a Codex session (CLI or desktop app) into a Claude Code session for
//! the same folder, so `claude --resume` continues it.
//!
//! Codex keeps each thread in `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`:
//! a `session_meta` line (id, cwd, git), `turn_context` lines (model), and
//! `response_item` lines in Responses API shape. A `compacted` line replaces
//! everything before it with Codex's own shortened history; we do the same.
//!
//! What carries over: your messages, the agent's replies, and each tool call
//! as text (command, patch, output). Hidden reasoning doesn't. The files in the
//! repo are the real state; the history tells the new agent how you got there.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, IsTerminal};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::resume::{alternate, clip, write_claude, Turn};

/// Per tool call, before the history gets squeezed to fit.
const MAX_TOOL_INPUT: usize = 1_500;
const MAX_TOOL_OUTPUT: usize = 1_500;
/// Total history we hand over (~100k tokens), leaving the new agent room to work.
const BUDGET: usize = 350_000;

/// Multi-agent plumbing: encrypted payloads, nothing a reader can use.
const SKIP_TOOLS: &[&str] = &["wait_agent", "send_message", "spawn_agent", "close_agent", "followup_task", "list_agents"];

pub struct CodexThread {
    pub id: String,
    pub path: PathBuf,
    pub cwd: String,
    pub title: String,
    pub modified: SystemTime,
}

fn codex_home() -> Result<PathBuf> {
    Ok(dirs::home_dir().context("no home dir")?.join(".codex"))
}

fn rollouts(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() {
            rollouts(&p, out);
        } else if p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("rollout-")) && p.extension().is_some_and(|x| x == "jsonl") {
            out.push(p);
        }
    }
}

/// Thread names Codex generated (`session_index.jsonl`, last entry wins).
fn thread_names() -> HashMap<String, String> {
    let mut names = HashMap::new();
    let Ok(text) = codex_home().and_then(|h| Ok(std::fs::read_to_string(h.join("session_index.jsonl"))?)) else { return names };
    for v in text.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()) {
        if let (Some(id), Some(name)) = (v["id"].as_str(), v["thread_name"].as_str()) {
            names.insert(id.to_string(), name.to_string());
        }
    }
    names
}

/// Your Codex threads, newest first. Sub-agent threads are left out.
pub fn codex_threads() -> Result<Vec<CodexThread>> {
    let mut files = vec![];
    rollouts(&codex_home()?.join("sessions"), &mut files);
    let names = thread_names();
    let mut out = vec![];
    for path in files {
        let Ok(f) = std::fs::File::open(&path) else { continue };
        let mut first = String::new();
        if BufReader::new(f).read_line(&mut first).is_err() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&first) else { continue };
        let p = &v["payload"];
        if v["type"] != "session_meta" || p["thread_source"] == "subagent" || p["source"].get("subagent").is_some() {
            continue;
        }
        let id = p["id"].as_str().unwrap_or_default().to_string();
        out.push(CodexThread {
            title: names.get(&id).cloned().unwrap_or_default(),
            cwd: p["cwd"].as_str().unwrap_or_default().to_string(),
            modified: std::fs::metadata(&path).and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH),
            id,
            path,
        });
    }
    out.sort_by(|a, b| b.modified.cmp(&a.modified));
    Ok(out)
}

/// The first thing you typed in a thread, for threads Codex never named.
fn first_prompt(path: &Path) -> Option<String> {
    let f = std::fs::File::open(path).ok()?;
    for line in BufReader::new(f).lines().map_while(Result::ok) {
        if !line.contains(r#""role":"user""#) {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        let text = crate::transcript::strip_reminders(&text_of(&v["payload"]["content"]));
        if !text.is_empty() && !crate::transcript::is_harness(&text) {
            return Some(text.split_whitespace().collect::<Vec<_>>().join(" "));
        }
    }
    None
}

/// The text of a Responses API `content` / `output` value.
fn text_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

/// What a tool call did, in one readable block.
fn tool_input(p: &Value) -> Option<(String, String)> {
    let name = p["name"].as_str().unwrap_or("tool").to_string();
    let input = match p["type"].as_str()? {
        "function_call" => {
            let raw = p["arguments"].as_str().unwrap_or_default();
            // exec_command-style calls: show just the command.
            serde_json::from_str::<Value>(raw).ok().and_then(|a| a["cmd"].as_str().map(str::to_string)).unwrap_or_else(|| raw.to_string())
        }
        "custom_tool_call" => p["input"].as_str().unwrap_or_default().to_string(),
        "local_shell_call" => p["action"]["command"].as_array().map(|c| c.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(" ")).unwrap_or_default(),
        "web_search_call" => return Some(("web_search".into(), p["action"]["query"].as_str().unwrap_or_default().to_string())),
        _ => return None,
    };
    if SKIP_TOOLS.contains(&name.as_str()) {
        return None;
    }
    Some((name, input))
}

/// One history entry; tools also keep a one-line form for when space runs out.
struct Item {
    role: &'static str,
    text: String,
    at: String,
    short: Option<String>,
}

fn push_item(items: &mut Vec<Item>, calls: &mut HashMap<String, usize>, p: &Value, at: &str) {
    match p["type"].as_str() {
        Some("message") => {
            let role = match p["role"].as_str() {
                Some("user") => "user",
                Some("assistant") => "assistant",
                _ => return, // developer / system instructions
            };
            let text = crate::transcript::strip_reminders(&text_of(&p["content"]));
            if text.is_empty() || crate::transcript::is_harness(&text) {
                return;
            }
            items.push(Item { role, text, at: at.to_string(), short: None });
        }
        Some("function_call_output" | "custom_tool_call_output") => {
            let Some(&i) = p["call_id"].as_str().and_then(|c| calls.get(c)) else { return };
            let out = text_of(&p["output"]);
            if !out.trim().is_empty() {
                items[i].text += &format!("\n→ {}", clip(out.trim(), MAX_TOOL_OUTPUT));
            }
        }
        _ => {
            let Some((name, input)) = tool_input(p) else { return };
            let input = input.trim();
            let first = input.lines().next().unwrap_or_default();
            if let Some(c) = p["call_id"].as_str() {
                calls.insert(c.to_string(), items.len());
            }
            items.push(Item {
                role: "assistant",
                text: format!("[tool: {name}] {}", clip(input, MAX_TOOL_INPUT)),
                at: at.to_string(),
                short: Some(format!("[tool: {name}] {}", clip(first, 160))),
            });
        }
    }
}

pub struct Converted {
    pub turns: Vec<Turn>,
    pub messages: usize,
    pub model: String,
    pub branch: String,
    pub cwd: String,
}

/// Read a Codex rollout into Claude-ready turns.
pub fn read_codex(path: &Path, title: &str) -> Result<Converted> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let (mut items, mut calls) = (Vec::new(), HashMap::new());
    let (mut model, mut branch, mut cwd, mut started) = (String::new(), String::new(), String::new(), String::new());
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let (p, at) = (&v["payload"], v["timestamp"].as_str().unwrap_or_default());
        match v["type"].as_str() {
            Some("session_meta") => {
                cwd = p["cwd"].as_str().unwrap_or_default().to_string();
                branch = p["git"]["branch"].as_str().unwrap_or_default().to_string();
                started = p["timestamp"].as_str().unwrap_or(at).to_string();
            }
            Some("turn_context") => {
                if let Some(m) = p["model"].as_str() {
                    model = m.to_string();
                }
            }
            Some("response_item") => push_item(&mut items, &mut calls, p, at),
            Some("compacted") => {
                // Codex swapped its history for this shorter one; follow it.
                items.clear();
                calls.clear();
                for it in p["replacement_history"].as_array().into_iter().flatten() {
                    push_item(&mut items, &mut calls, it, at);
                }
                let summary = p["message"].as_str().unwrap_or_default().trim();
                if !summary.is_empty() {
                    items.push(Item { role: "user", text: format!("[Codex's summary of the earlier conversation]\n{summary}"), at: at.to_string(), short: None });
                }
            }
            _ => {}
        }
    }
    let messages = items.len();
    let dropped = fit(&mut items);
    let mut note = format!(
        "[Continued from a Codex session{}{}, started {}. Tool calls appear as text; the files in this folder are the current state.]",
        if title.is_empty() { String::new() } else { format!(" \"{title}\"") },
        if model.is_empty() { String::new() } else { format!(" ({model})") },
        started.get(..10).unwrap_or("earlier"),
    );
    if dropped > 0 {
        note += &format!("\n[{dropped} earlier messages were left out to fit.]");
    }
    let turns = alternate(items.into_iter().map(|i| (i.role, i.text, i.at)).collect(), &note);
    Ok(Converted { turns, messages, model, branch, cwd })
}

/// Squeeze the history under BUDGET: first shorten old tool calls to one line,
/// then drop the oldest messages. Returns how many were dropped.
fn fit(items: &mut Vec<Item>) -> usize {
    let size = |items: &[Item]| items.iter().map(|i| i.text.len()).sum::<usize>();
    let mut total = size(items);
    for it in items.iter_mut() {
        if total <= BUDGET {
            return 0;
        }
        if let Some(short) = it.short.take() {
            total -= it.text.len() - short.len().min(it.text.len());
            it.text = short;
        }
    }
    let mut dropped = 0;
    while total > BUDGET && items.len() > 1 {
        total -= items.remove(0).text.len();
        dropped += 1;
    }
    dropped
}

fn ago(t: SystemTime) -> String {
    let s = SystemTime::now().duration_since(t).map(|d| d.as_secs()).unwrap_or(0);
    match s {
        s if s < 3_600 => format!("{}m ago", s / 60),
        s if s < 86_400 => format!("{}h ago", s / 3_600),
        s => format!("{}d ago", s / 86_400),
    }
}

/// `pn handoff <to> [session] [--all]`.
pub fn run(to: &str, session: Option<String>, all: bool) -> Result<()> {
    match to {
        "claude" => codex_to_claude(session, all),
        "codex" => claude_to_codex(session, all),
        other => bail!("handoff goes to claude or codex, not {other}"),
    }
}

/// Which session to move: a list number, an id (or its start), or a file.
/// With nothing given, show this folder's sessions and ask (Enter = newest).
fn pick(
    mut threads: Vec<CodexThread>,
    everything: fn() -> Result<Vec<CodexThread>>,
    session: Option<String>,
    all: bool,
    here: &str,
    agent: &str,
) -> Result<CodexThread> {
    match session {
        Some(s) if Path::new(&s).is_file() => {
            let path = PathBuf::from(&s).canonicalize()?;
            let found = everything()?.into_iter().find(|t| t.path.canonicalize().is_ok_and(|p| p == path));
            found.with_context(|| format!("{s} isn't a {agent} session file"))
        }
        Some(s) => match s.parse::<usize>().ok().filter(|n| *n >= 1 && *n <= threads.len()) {
            Some(n) => Ok(threads.swap_remove(n - 1)),
            None => everything()?.into_iter().find(|t| t.id.starts_with(&s)).with_context(|| format!("no {agent} session {s}")),
        },
        None => {
            if threads.is_empty() {
                bail!("No {agent} sessions in {here}. Run this inside the folder you used {agent} in, or add --all.");
            }
            println!("{agent} sessions{}:", if all { String::new() } else { format!(" in {here}") });
            for (i, t) in threads.iter().take(15).enumerate() {
                let title = if t.title.is_empty() { first_prompt(&t.path).unwrap_or_else(|| t.id.clone()) } else { t.title.clone() };
                let place = if all { format!("  {}", t.cwd) } else { String::new() };
                println!("  {:>2}  {:>8}  {}{place}", i + 1, ago(t.modified), clip(&title, 60));
            }
            let n = if std::io::stdin().is_terminal() {
                let a = crate::team::ask("Which one? [1] ").unwrap_or_default();
                if a.is_empty() { 1 } else { a.parse::<usize>().context("type a number from the list")? }
            } else {
                1
            };
            if n == 0 || n > threads.len().min(15) {
                bail!("pick a number from 1 to {}", threads.len().min(15));
            }
            Ok(threads.swap_remove(n - 1))
        }
    }
}

fn codex_to_claude(session: Option<String>, all: bool) -> Result<()> {
    let here = std::env::current_dir()?;
    let here_s = here.to_string_lossy().to_string();
    let mut threads = codex_threads()?;
    if !all {
        threads.retain(|t| t.cwd == here_s);
    }
    let pick = pick(threads, codex_threads, session, all, &here_s, "Codex")?;

    let c = read_codex(&pick.path, &pick.title)?;
    if c.turns.len() < 2 {
        bail!("that Codex session has no conversation yet");
    }
    // Resume where the code is: the Codex folder if it still exists here.
    let cwd = Some(PathBuf::from(&c.cwd)).filter(|p| !c.cwd.is_empty() && p.is_dir()).unwrap_or(here.clone());
    let title = if pick.title.is_empty() { format!("From Codex: {}", clip(&first_prompt(&pick.path).unwrap_or_default(), 60)) } else { format!("From Codex: {}", pick.title) };
    let sid = write_claude(c.turns, &title, &c.model, &c.branch, &cwd)?;

    println!("✓ Added to Claude Code: {title} ({} messages)", c.messages);
    let cd = if cwd != here { format!("cd {} && ", cwd.display()) } else { String::new() };
    println!("  Continue it:  {cd}pn -- --resume {sid}     (or: {cd}claude --resume {sid})");
    println!("  It's also in the list from `claude --resume` and the Claude desktop app.");
    if cwd == here && std::io::stdin().is_terminal() && crate::team::ask("Open it now? [Y/n] ").is_some_and(|a| a.is_empty() || a.eq_ignore_ascii_case("y")) {
        // Through pn, so the continued conversation is recorded too.
        let status = std::process::Command::new(std::env::current_exe()?).args(["--", "--resume", &sid]).status()?;
        std::process::exit(status.code().unwrap_or(0));
    }
    Ok(())
}


// ── Claude Code → Codex ─────────────────────────────────────────────────────

/// Folder, title and "has Claude replied?" from a quick pass over a Claude
/// Code session file. A file with no real reply is a copy pn made from Codex.
fn scan_claude(path: &Path) -> Option<(String, String, bool)> {
    let f = std::fs::File::open(path).ok()?;
    let (mut cwd, mut title, mut first, mut real) = (String::new(), String::new(), String::new(), false);
    for line in BufReader::new(f).lines().map_while(Result::ok) {
        if line.contains(r#""type":"ai-title""#) {
            if let Some(t) = serde_json::from_str::<Value>(&line).ok().and_then(|v| v["aiTitle"].as_str().map(str::to_string)) {
                title = t;
            }
        } else if line.contains(r#""type":"assistant""#) && !line.contains("msg_pine_") && !line.contains(r#""isSidechain":true"#) {
            real = true;
        } else if (cwd.is_empty() || first.is_empty()) && line.contains(r#""type":"user""#) {
            let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
            if cwd.is_empty() {
                cwd = v["cwd"].as_str().unwrap_or_default().to_string();
            }
            if first.is_empty() && v["isMeta"] != true {
                let text = crate::transcript::strip_reminders(&text_of(&v["message"]["content"]));
                if !text.is_empty() && !crate::transcript::is_harness(&text) {
                    first = text.split_whitespace().collect::<Vec<_>>().join(" ");
                }
            }
        }
    }
    Some((cwd, if title.is_empty() { first } else { title }, real))
}

/// Your Claude Code sessions, newest first (every folder; filter by cwd).
pub fn claude_threads() -> Result<Vec<CodexThread>> {
    let root = dirs::home_dir().context("no home dir")?.join(".claude/projects");
    let mut out = vec![];
    for proj in std::fs::read_dir(root).into_iter().flatten().flatten() {
        for f in std::fs::read_dir(proj.path()).into_iter().flatten().flatten() {
            let path = f.path();
            if path.extension().is_none_or(|x| x != "jsonl") {
                continue;
            }
            let Some((cwd, title, real)) = scan_claude(&path) else { continue };
            if !real {
                continue;
            }
            out.push(CodexThread {
                id: path.file_stem().unwrap_or_default().to_string_lossy().to_string(),
                modified: std::fs::metadata(&path).and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH),
                cwd,
                title,
                path,
            });
        }
    }
    out.sort_by(|a, b| b.modified.cmp(&a.modified));
    Ok(out)
}

/// A Claude Code session → Codex-ready turns. Follows Claude's own /compact
/// (a compact summary replaces what came before); sub-agents and thinking are left out.
pub fn read_claude(path: &Path, title: &str) -> Result<Converted> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let (mut items, mut calls): (Vec<Item>, HashMap<String, usize>) = (Vec::new(), HashMap::new());
    let (mut model, mut branch, mut cwd, mut started) = (String::new(), String::new(), String::new(), String::new());
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let role: &'static str = match v["type"].as_str() {
            Some("user") => "user",
            Some("assistant") => "assistant",
            _ => continue,
        };
        if v["isSidechain"] == true || v["isMeta"] == true {
            continue;
        }
        let at = v["timestamp"].as_str().unwrap_or_default();
        if cwd.is_empty() {
            cwd = v["cwd"].as_str().unwrap_or_default().to_string();
            started = at.to_string();
        }
        if let Some(b) = v["gitBranch"].as_str().filter(|b| !b.is_empty()) {
            branch = b.to_string();
        }
        let msg = &v["message"];
        if let Some(m) = msg["model"].as_str().filter(|m| m.starts_with("claude")) {
            model = m.to_string();
        }
        if v["isCompactSummary"] == true {
            items.clear();
            calls.clear();
            items.push(Item { role: "user", text: format!("[Claude's summary of the earlier conversation]\n{}", text_of(&msg["content"])), at: at.to_string(), short: None });
            continue;
        }
        let blocks = match &msg["content"] {
            Value::String(s) => vec![json!({ "type": "text", "text": s })],
            Value::Array(a) => a.clone(),
            _ => continue,
        };
        for b in blocks {
            match b["type"].as_str() {
                Some("text") => {
                    let t = crate::transcript::strip_reminders(b["text"].as_str().unwrap_or_default());
                    if !t.is_empty() && !crate::transcript::is_harness(&t) {
                        items.push(Item { role, text: t, at: at.to_string(), short: None });
                    }
                }
                Some("tool_use") => {
                    let name = b["name"].as_str().unwrap_or("tool");
                    let inp = &b["input"];
                    // Bash: the command; file tools: the path; otherwise the JSON.
                    let input = inp["command"].as_str().map(str::to_string).unwrap_or_else(|| match inp["file_path"].as_str() {
                        Some(p) if name != "Edit" && name != "Write" => p.to_string(),
                        _ => inp.to_string(),
                    });
                    if let Some(id) = b["id"].as_str() {
                        calls.insert(id.to_string(), items.len());
                    }
                    items.push(Item {
                        role: "assistant",
                        text: format!("[tool: {name}] {}", clip(input.trim(), MAX_TOOL_INPUT)),
                        at: at.to_string(),
                        short: Some(format!("[tool: {name}] {}", clip(input.trim().lines().next().unwrap_or_default(), 160))),
                    });
                }
                Some("tool_result") => {
                    let Some(&i) = b["tool_use_id"].as_str().and_then(|c| calls.get(c)) else { continue };
                    let out = text_of(&b["content"]);
                    if !out.trim().is_empty() {
                        items[i].text += &format!("\n→ {}", clip(out.trim(), MAX_TOOL_OUTPUT));
                    }
                }
                _ => {} // thinking, images
            }
        }
    }
    let messages = items.len();
    let dropped = fit(&mut items);
    let mut note = format!(
        "[Continued from a Claude Code session{}{}, started {}. Tool calls appear as text; the files in this folder are the current state.]",
        if title.is_empty() { String::new() } else { format!(" \"{title}\"") },
        if model.is_empty() { String::new() } else { format!(" ({model})") },
        started.get(..10).unwrap_or("earlier"),
    );
    if dropped > 0 {
        note += &format!("\n[{dropped} earlier messages were left out to fit.]");
    }
    let turns = alternate(items.into_iter().map(|i| (i.role, i.text, i.at)).collect(), &note);
    Ok(Converted { turns, messages, model, branch, cwd })
}

fn claude_to_codex(session: Option<String>, all: bool) -> Result<()> {
    let here = std::env::current_dir()?;
    let here_s = here.to_string_lossy().to_string();
    let mut threads = claude_threads()?;
    if !all {
        threads.retain(|t| t.cwd == here_s);
    }
    let pick = pick(threads, claude_threads, session, all, &here_s, "Claude Code")?;
    let c = read_claude(&pick.path, &pick.title)?;
    if c.turns.len() < 2 {
        bail!("that Claude Code session has no conversation yet");
    }
    let cwd = Some(PathBuf::from(&c.cwd)).filter(|p| !c.cwd.is_empty() && p.is_dir()).unwrap_or(here.clone());
    let title = format!("From Claude: {}", clip(if pick.title.is_empty() { "conversation" } else { &pick.title }, 60));
    let id = crate::into_codex::write_codex(c.turns, &title, &cwd)?;

    println!("✓ Added to Codex: {title} ({} messages)", c.messages);
    let cd = if cwd != here { format!("cd {} && ", cwd.display()) } else { String::new() };
    println!("  Continue it:  {cd}pn codex -- resume {id}     (or: {cd}codex resume {id})");
    println!("  It's also in `codex resume` and the Codex desktop app (restart the app if it's open).");
    if cwd == here && std::io::stdin().is_terminal() && crate::team::ask("Open it now? [Y/n] ").is_some_and(|a| a.is_empty() || a.eq_ignore_ascii_case("y")) {
        // Through pn, so the continued conversation is recorded too.
        let status = std::process::Command::new(std::env::current_exe()?).args(["codex", "--", "resume", &id]).status()?;
        std::process::exit(status.code().unwrap_or(0));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_rollout_with_tools_and_compaction() {
        let dir = std::env::temp_dir().join(format!("pine-handoff-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("rollout-x.jsonl");
        let lines = [
            r#"{"timestamp":"2026-10-01T10:00:00Z","type":"session_meta","payload":{"id":"abc","timestamp":"2026-10-01T10:00:00Z","cwd":"/tmp/repo","git":{"branch":"main"}}}"#,
            r#"{"timestamp":"t","type":"turn_context","payload":{"model":"gpt-x"}}"#,
            r#"{"timestamp":"t","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"old question"}]}}"#,
            r#"{"timestamp":"t","type":"compacted","payload":{"message":"","replacement_history":[{"type":"message","role":"user","content":[{"type":"input_text","text":"build the page"}]}]}}"#,
            r#"{"timestamp":"t","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>cwd</environment_context>"}]}}"#,
            r#"{"timestamp":"t","type":"response_item","payload":{"type":"reasoning","summary":[]}}"#,
            r#"{"timestamp":"t","type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"c1","arguments":"{\"cmd\":\"ls\"}"}}"#,
            r#"{"timestamp":"t","type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":"index.html"}}"#,
            r#"{"timestamp":"t","type":"response_item","payload":{"type":"function_call","name":"wait_agent","call_id":"c2","arguments":"{}"}}"#,
            r#"{"timestamp":"t","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Done."}]}}"#,
        ];
        std::fs::write(&f, lines.join("\n")).unwrap();
        let c = read_codex(&f, "Landing page").unwrap();
        assert_eq!((c.model.as_str(), c.branch.as_str(), c.cwd.as_str()), ("gpt-x", "main", "/tmp/repo"));
        let roles: Vec<_> = c.turns.iter().map(|t| t.0).collect();
        assert_eq!(roles, ["user", "assistant"]);
        assert!(c.turns[0].1.starts_with("[Continued from a Codex session \"Landing page\" (gpt-x), started 2026-10-01."));
        assert!(c.turns[0].1.ends_with("build the page"));
        assert!(!c.turns[0].1.contains("old question"));
        assert_eq!(c.turns[1].1, "[tool: exec_command] ls\n→ index.html\n\nDone.");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn fit_shortens_tools_before_dropping() {
        let big = "x".repeat(BUDGET);
        let mut items = vec![
            Item { role: "assistant", text: format!("[tool: exec] {big}"), at: String::new(), short: Some("[tool: exec] x".into()) },
            Item { role: "user", text: "keep me".into(), at: String::new(), short: None },
        ];
        assert_eq!(fit(&mut items), 0);
        assert_eq!(items[0].text, "[tool: exec] x");
    }
}

#[cfg(test)]
mod real {
    /// Converts your own newest Codex threads without writing anything:
    /// `cargo test real_codex -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn real_codex() {
        let mut all = super::codex_threads().unwrap();
        all.sort_by_key(|t| std::cmp::Reverse(std::fs::metadata(&t.path).map(|m| m.len()).unwrap_or(0)));
        for t in all.iter().take(3) {
            let c = super::read_codex(&t.path, &t.title).unwrap();
            let chars: usize = c.turns.iter().map(|x| x.1.len()).sum();
            println!("{} | {} | {} msgs -> {} turns, {} chars | {}", t.id, t.cwd, c.messages, c.turns.len(), chars, t.title);
            if let Some(last) = c.turns.last() {
                println!("   last {}: {}", last.0, super::clip(&last.1.replace('\n', " "), 140));
            }
        }
    }
}

#[cfg(test)]
mod real_claude {
    /// Reads your own newest Claude Code sessions, writes nothing:
    /// `cargo test real_claude_sessions -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn real_claude_sessions() {
        let all = super::claude_threads().unwrap();
        println!("{} Claude Code sessions with real replies", all.len());
        for t in all.iter().take(4) {
            let c = super::read_claude(&t.path, &t.title).unwrap();
            let chars: usize = c.turns.iter().map(|x| x.1.len()).sum();
            println!("{} | {} | {} msgs -> {} turns, {chars} chars | {}", &t.id[..8], t.cwd, c.messages, c.turns.len(), super::clip(&t.title, 50));
            println!("   first: {}", super::clip(&c.turns[0].1.replace('\n', " "), 150));
        }
    }
}
