//! Beta: Pine inside Claude Code. Teammates' live work shows in Claude Code's
//! status line, and Claude itself is told (through hooks) when a teammate's
//! agent touched files in the same repo.
//!
//! `pn beta hooks` adds to ~/.claude/settings.json (and `--remove` takes out):
//!   statusLine        → `pn beta statusline`        one line under the input box
//!   SessionStart      → `pn beta hook session-start` who's live, recent edits
//!   UserPromptSubmit  → `pn beta hook prompt`        edits since Claude last heard
//!   PostToolUse(Edit) → `pn beta hook edit`          report the edit; warn on overlap
//!
//! Hooks and the status line never fail Claude: no login, no repo, offline or
//! slow all end quietly. Everything here follows the web's rule: you only hear
//! about repos you can read, and never about private sessions.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::team::{api_base, git, github_repo, load_auth, Auth};

const MARK: &str = "beta hook"; //  how our hook commands are recognised
const CACHE_FOR: u64 = 10_000; //   ms between status line fetches
const RECENT: u64 = 30 * 60_000; // ms: "recent" edits

// ── install / remove ────────────────────────────────────────────────────────

fn settings_path() -> Result<PathBuf> {
    Ok(dirs::home_dir().context("no home folder")?.join(".claude/settings.json"))
}

fn prev_statusline_path() -> Result<PathBuf> {
    Ok(crate::paths::root()?.join("statusline-prev.json"))
}

/// The command that runs this pn, quoted for the shell (hooks may not have ~/.local/bin on PATH).
fn me() -> Result<String> {
    let exe = std::env::current_exe()?;
    Ok(format!("'{}'", exe.to_string_lossy().replace('\'', r"'\''")))
}

/// `pn beta hooks [--remove]`
pub fn hooks_cmd(remove: bool) -> Result<()> {
    let path = settings_path()?;
    let mut s: Value = match std::fs::read(&path) {
        Ok(b) => serde_json::from_slice(&b).with_context(|| format!("{} isn't valid JSON", path.display()))?,
        Err(_) => json!({}),
    };
    if !s.is_object() {
        anyhow::bail!("{} isn't a JSON object", path.display());
    }
    // One backup of the file as it was before Pine first touched it.
    let backup = path.with_extension("json.pine-backup");
    if path.exists() && !backup.exists() {
        std::fs::copy(&path, &backup)?;
    }

    strip_ours(&mut s);
    let prev = prev_statusline_path()?;
    let ours = |v: &Value| v["command"].as_str().is_some_and(|c| c.contains("beta statusline"));

    if remove {
        // Give back the status line that was there before, if any.
        match std::fs::read(&prev).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()) {
            Some(old) => s["statusLine"] = old,
            None if ours(&s["statusLine"]) => {
                s.as_object_mut().unwrap().remove("statusLine");
            }
            None => {}
        }
        let _ = std::fs::remove_file(&prev);
        write(&path, &s)?;
        println!("Removed Pine's status line and hooks from {}.", path.display());
        return Ok(());
    }

    let pn = me()?;
    // Keep someone's own status line: Pine shows it first, then its own line.
    if let Some(old) = s.get("statusLine").filter(|v| !v.is_null() && !ours(v)) {
        std::fs::write(&prev, serde_json::to_vec_pretty(old)?)?;
    }
    s["statusLine"] = json!({ "type": "command", "command": format!("{pn} beta statusline"), "refreshInterval": 15 });

    let hook = |kind: &str, extra: Value| {
        let mut h = json!({ "type": "command", "command": format!("{pn} {MARK} {kind}"), "timeout": 10 });
        if let (Some(h), Some(e)) = (h.as_object_mut(), extra.as_object()) {
            h.extend(e.clone());
        }
        h
    };
    let hooks = s["hooks"].as_object().cloned().unwrap_or_default();
    let mut hooks = Value::Object(hooks);
    push(&mut hooks, "SessionStart", json!({ "matcher": "startup|resume|clear", "hooks": [hook("session-start", json!({}))] }));
    push(&mut hooks, "UserPromptSubmit", json!({ "hooks": [hook("prompt", json!({}))] }));
    push(&mut hooks, "PostToolUse", json!({ "matcher": "Edit|Write|MultiEdit|NotebookEdit", "hooks": [hook("edit", json!({}))] }));
    s["hooks"] = hooks;
    write(&path, &s)?;

    println!("Pine is in Claude Code (beta). In a GitHub repo you'll see teammates' live work under the input box,");
    println!("and Claude hears when their agents edit files you're working on. Start a new `claude` (or `pn`) to see it.");
    println!("  Settings: {}", path.display());
    println!("  Undo:     pn beta hooks --remove");
    if load_auth().is_none() {
        println!("  Not logged in yet: run `pn login` first, or there's nothing to show.");
    }
    Ok(())
}

fn push(hooks: &mut Value, event: &str, group: Value) {
    let list = hooks[event].as_array().cloned().unwrap_or_default();
    let mut list = list;
    list.push(group);
    hooks[event] = Value::Array(list);
}

/// Take out every hook group Pine added earlier (so installing twice doesn't double up).
fn strip_ours(s: &mut Value) {
    let Some(hooks) = s["hooks"].as_object_mut() else { return };
    for (_, groups) in hooks.iter_mut() {
        if let Some(list) = groups.as_array_mut() {
            list.retain(|g| !g["hooks"].as_array().is_some_and(|hs| hs.iter().any(|h| h["command"].as_str().is_some_and(|c| c.contains(MARK)))));
        }
    }
    hooks.retain(|_, v| !v.as_array().is_some_and(|l| l.is_empty()));
}

fn write(path: &Path, v: &Value) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, serde_json::to_vec_pretty(v)?)?;
    Ok(())
}

// ── activity from Pine ──────────────────────────────────────────────────────

#[derive(Debug, Default, Deserialize, serde::Serialize, Clone)]
struct Activity {
    #[serde(default)]
    live: Vec<Live>,
    #[serde(default)]
    edits: Vec<Edit>,
}
#[derive(Debug, Deserialize, serde::Serialize, Clone)]
struct Live {
    login: String,
    agent: String,
    title: Option<String>,
    #[serde(rename = "lastActivityAt")]
    last: String,
}
#[derive(Debug, Deserialize, serde::Serialize, Clone)]
struct Edit {
    login: String,
    agent: String,
    path: String,
    at: String,
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn ms_of(iso: &str) -> u64 {
    time::OffsetDateTime::parse(iso, &time::format_description::well_known::Rfc3339)
        .map(|t| (t.unix_timestamp_nanos() / 1_000_000) as u64)
        .unwrap_or(0)
}

fn ago(iso: &str) -> String {
    let s = now_ms().saturating_sub(ms_of(iso)) / 1000;
    match s {
        0..=59 => "just now".into(),
        60..=3599 => format!("{}m ago", s / 60),
        _ => format!("{}h ago", s / 3600),
    }
}

fn agent_name(a: &str) -> &str {
    if a == "codex" {
        "Codex"
    } else {
        "Claude Code"
    }
}

/// The GitHub repo a folder belongs to, and the folder's repo root.
fn repo_of(cwd: &str) -> Option<(String, String)> {
    let repo = github_repo(&git(cwd, &["remote", "get-url", "origin"])?)?;
    let root = git(cwd, &["rev-parse", "--show-toplevel"])?;
    Some((repo, root))
}

fn cache_path(repo: &str) -> Option<PathBuf> {
    Some(crate::paths::root().ok()?.join("activity").join(format!("{}.json", repo.replace('/', "__"))))
}

fn http() -> Option<reqwest::Client> {
    reqwest::Client::builder().timeout(Duration::from_secs(4)).user_agent(format!("pine/{}", crate::PINE_VERSION)).build().ok()
}

fn rt() -> Option<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread().enable_all().build().ok()
}

/// Teammates' activity in a repo, fetched at most every 10 s (shared by the status line and hooks).
fn activity(auth: &Auth, repo: &str, fresh: bool) -> Activity {
    let path = cache_path(repo);
    if let Some(p) = &path {
        if let Ok(meta) = std::fs::metadata(p) {
            let age = meta.modified().ok().and_then(|m| m.elapsed().ok()).map(|d| d.as_millis() as u64).unwrap_or(u64::MAX);
            if !fresh && age < CACHE_FOR {
                if let Some(a) = std::fs::read(p).ok().and_then(|b| serde_json::from_slice(&b).ok()) {
                    return a;
                }
            }
        }
    }
    let fetched = rt().and_then(|rt| {
        rt.block_on(async {
            let res = http()?
                .get(format!("{}/api/v1/activity?repo={repo}", api_base(Some(auth))))
                .bearer_auth(&auth.token)
                .send()
                .await
                .ok()?;
            if !res.status().is_success() {
                return None;
            }
            res.json::<Activity>().await.ok()
        })
    });
    match (fetched, &path) {
        (Some(a), Some(p)) => {
            let _ = std::fs::create_dir_all(p.parent().unwrap());
            let _ = std::fs::write(p, serde_json::to_vec(&a).unwrap_or_default());
            a
        }
        (Some(a), None) => a,
        // Offline or slow: whatever we had last.
        (None, Some(p)) => std::fs::read(p).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default(),
        (None, None) => Activity::default(),
    }
}

fn stdin_json() -> Value {
    let mut s = String::new();
    let _ = std::io::stdin().read_to_string(&mut s);
    serde_json::from_str(&s).unwrap_or(Value::Null)
}

// ── status line ─────────────────────────────────────────────────────────────

/// `pn beta statusline`: one line (after any status line you had before) about teammates in this repo.
pub fn statusline() -> Result<()> {
    let mut raw = String::new();
    let _ = std::io::stdin().read_to_string(&mut raw);
    let input: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);

    // Your own status line first, if Pine replaced one.
    if let Some(cmd) = prev_statusline_path().ok().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice::<Value>(&b).ok()).and_then(|v| v["command"].as_str().map(str::to_string)) {
        if let Ok(mut child) = std::process::Command::new("sh").arg("-c").arg(cmd).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).spawn() {
            if let Some(mut stdin) = child.stdin.take() {
                use std::io::Write;
                let _ = stdin.write_all(raw.as_bytes());
            }
            if let Ok(out) = child.wait_with_output() {
                print!("{}", String::from_utf8_lossy(&out.stdout));
            }
        }
    }

    let cwd = input["workspace"]["current_dir"].as_str().or_else(|| input["cwd"].as_str()).unwrap_or(".").to_string();
    let (Some(auth), Some((repo, _))) = (load_auth(), repo_of(&cwd)) else { return Ok(()) };
    let a = activity(&auth, &repo, false);
    println!("{}", status_text(&repo, &a));
    Ok(())
}

const GREEN: &str = "\x1b[32m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

fn status_text(repo: &str, a: &Activity) -> String {
    let recent: Vec<&Edit> = a.edits.iter().filter(|e| now_ms().saturating_sub(ms_of(&e.at)) < RECENT).collect();
    if a.live.is_empty() && recent.is_empty() {
        return format!("{GREEN}🌲{RESET} {DIM}Pine · nobody else active in {repo}{RESET}");
    }
    let mut parts = vec![];
    match a.live.as_slice() {
        [] => {}
        [one] => parts.push(format!(
            "{} live in {}{}",
            one.login,
            agent_name(&one.agent),
            one.title.as_deref().map(|t| format!(" · “{}”", short(t, 40))).unwrap_or_default()
        )),
        many => parts.push(format!("{} live: {}", many.len(), many.iter().map(|l| l.login.as_str()).take(4).collect::<Vec<_>>().join(", "))),
    }
    if let Some(e) = recent.first() {
        parts.push(format!("{} edited {} {}", e.login, short(&e.path, 40), ago(&e.at)));
    }
    format!("{GREEN}🌲{RESET} {}", parts.join(&format!(" {DIM}·{RESET} ")))
}

fn short(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n - 1).collect::<String>())
    }
}

// ── hooks ───────────────────────────────────────────────────────────────────

/// `pn beta hook <session-start|prompt|edit>`: reads Claude Code's hook JSON on stdin.
pub fn hook(kind: &str) -> Result<()> {
    let input = stdin_json();
    let cwd = input["cwd"].as_str().unwrap_or(".").to_string();
    let session = input["session_id"].as_str().unwrap_or("").to_string();
    let (Some(auth), Some((repo, root))) = (load_auth(), repo_of(&cwd)) else { return Ok(()) };
    match kind {
        "session-start" => {
            let a = activity(&auth, &repo, true);
            if let Some(text) = context_text(&repo, &a.live, &recent_edits(&a, 0)) {
                println!("{text}"); // SessionStart: plain stdout becomes context Claude sees
            }
            mark_seen(&session);
        }
        "prompt" => {
            let a = activity(&auth, &repo, false);
            let edits = recent_edits(&a, last_seen(&session));
            if !edits.is_empty() {
                if let Some(text) = context_text(&repo, &[], &edits) {
                    println!("{text}"); // UserPromptSubmit: same
                }
            }
            mark_seen(&session);
        }
        "edit" => {
            let file = input["tool_input"]["file_path"].as_str().or_else(|| input["tool_input"]["notebook_path"].as_str()).unwrap_or("");
            let Some(rel) = relative(&root, file) else { return Ok(()) };
            report_edit(&auth, &repo, &rel, &session);
            // Did a teammate's agent touch this same file lately? Tell Claude.
            let a = activity(&auth, &repo, false);
            if let Some(e) = recent_edits(&a, 0).into_iter().find(|e| e.path == rel) {
                let msg = format!(
                    "Pine: {}'s {} agent also edited {} {}. Their change may differ from what you just wrote. Check the file still has their work, and tell the user about the overlap.",
                    e.login,
                    agent_name(&e.agent),
                    rel,
                    ago(&e.at)
                );
                println!("{}", json!({ "hookSpecificOutput": { "hookEventName": "PostToolUse", "additionalContext": msg } }));
            }
        }
        _ => {}
    }
    Ok(())
}

fn recent_edits(a: &Activity, since_ms: u64) -> Vec<Edit> {
    let cutoff = since_ms.max(now_ms().saturating_sub(RECENT));
    a.edits.iter().filter(|e| ms_of(&e.at) > cutoff).cloned().collect()
}

/// What Claude is told: who's live in the repo and which files their agents changed.
fn context_text(repo: &str, live: &[Live], edits: &[Edit]) -> Option<String> {
    if live.is_empty() && edits.is_empty() {
        return None;
    }
    let mut out = format!("Pine (shared activity in {repo}, from teammates' AI sessions):\n");
    for l in live {
        out.push_str(&format!(
            "- {} is working in {}{} (active {}).\n",
            l.login,
            agent_name(&l.agent),
            l.title.as_deref().map(|t| format!(" on “{t}”")).unwrap_or_default(),
            ago(&l.last)
        ));
    }
    if !edits.is_empty() {
        out.push_str("Files their agents edited recently:\n");
        let mut seen = std::collections::HashSet::new();
        for e in edits.iter().filter(|e| seen.insert(e.path.clone())).take(15) {
            out.push_str(&format!("- {} ({}, {})\n", e.path, e.login, ago(&e.at)));
        }
        out.push_str("If you read or change these files, re-read them first: they may have changed since you last saw them. Mention it to the user when it matters.");
    }
    Some(out)
}

/// The file's path inside the repo ("src/auth.ts"), or None if it's outside.
fn relative(root: &str, file: &str) -> Option<String> {
    let direct = Path::new(file).strip_prefix(root).ok().map(Path::to_path_buf);
    // macOS spells some folders two ways (/var vs /private/var): compare real paths too.
    let rel = direct.or_else(|| {
        let (r, f) = (std::fs::canonicalize(root).ok()?, std::fs::canonicalize(file).ok()?);
        f.strip_prefix(&r).ok().map(Path::to_path_buf)
    })?;
    let s = rel.to_string_lossy().to_string();
    (!s.is_empty() && !s.starts_with("..")).then_some(s)
}

fn report_edit(auth: &Auth, repo: &str, path: &str, session: &str) {
    let body = json!({ "repo": repo, "path": path, "session": crate::native::ulid_of(session), "agent": "claude" });
    if let (Some(rt), Some(c)) = (rt(), http()) {
        rt.block_on(async {
            let _ = c.post(format!("{}/api/v1/activity", api_base(Some(auth)))).bearer_auth(&auth.token).json(&body).send().await;
        });
    }
}

/// When this Claude session last heard about teammates' edits.
fn seen_path(session: &str) -> Option<PathBuf> {
    (!session.is_empty() && session.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')).then(|| crate::paths::root().ok()).flatten().map(|r| r.join("hook-state").join(session))
}

fn last_seen(session: &str) -> u64 {
    seen_path(session).and_then(|p| std::fs::read_to_string(p).ok()).and_then(|s| s.trim().parse().ok()).unwrap_or(0)
}

fn mark_seen(session: &str) {
    if let Some(p) = seen_path(session) {
        let _ = std::fs::create_dir_all(p.parent().unwrap());
        let _ = std::fs::write(p, now_ms().to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installing_twice_keeps_one_copy_and_others_hooks() {
        let mut hooks = json!({ "PostToolUse": [{ "matcher": "Bash", "hooks": [{ "type": "command", "command": "my-linter" }] }] });
        for _ in 0..2 {
            let mut s = json!({ "hooks": hooks.clone() });
            strip_ours(&mut s);
            hooks = s["hooks"].clone();
            push(&mut hooks, "PostToolUse", json!({ "matcher": "Edit", "hooks": [{ "type": "command", "command": "'/x/pn' beta hook edit" }] }));
        }
        let list = hooks["PostToolUse"].as_array().unwrap();
        assert_eq!(list.len(), 2, "the user's own hook + one of ours");
        assert_eq!(list[0]["hooks"][0]["command"], "my-linter");

        let mut s = json!({ "hooks": hooks });
        strip_ours(&mut s);
        assert_eq!(s["hooks"]["PostToolUse"].as_array().unwrap().len(), 1, "remove takes only ours");
    }

    #[test]
    fn nothing_to_say_when_nobody_is_around() {
        assert!(context_text("a/b", &[], &[]).is_none());
        let e = Edit { login: "jialu".into(), agent: "codex".into(), path: "src/auth.ts".into(), at: "2026-01-01T00:00:00Z".into() };
        let t = context_text("a/b", &[], &[e.clone(), e]).unwrap();
        assert_eq!(t.matches("src/auth.ts").count(), 1, "each file once");
    }

    #[test]
    fn paths_inside_the_repo_only() {
        assert_eq!(relative("/r", "/r/src/a.ts").as_deref(), Some("src/a.ts"));
        assert_eq!(relative("/r", "/elsewhere/a.ts"), None);
    }
}
