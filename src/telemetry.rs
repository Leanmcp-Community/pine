//! Usage events to PostHog: which command ran, never what it ran on.
//!
//! One `cli_command` event per run with the command name and a few safe
//! flags (e.g. `into: codex`), the pn version, OS and arch. Never arguments,
//! paths, prompts, code or session contents. Logged in, the person is your
//! GitHub login (the same id the web app uses); otherwise a random id kept in
//! `~/.pine/telemetry-id`.
//!
//! Off: `pn telemetry off`, or `PINE_TELEMETRY=0` / `DO_NOT_TRACK=1`.
//! Sending runs on its own thread and never delays a command; at exit we wait
//! at most a second for it.

use std::sync::mpsc;
use std::time::Duration;

use anyhow::Result;
use serde_json::{json, Value};

/// Our CloudFront proxy in front of PostHog US (same project as the web app).
const HOST: &str = "https://d18m0xvdtnkibr.cloudfront.net";
const KEY: &str = env!("POSTHOG_API_KEY");

fn off_marker() -> Option<std::path::PathBuf> {
    crate::paths::root().ok().map(|r| r.join("telemetry-off"))
}

pub fn enabled() -> bool {
    let env_off = |k: &str| std::env::var(k).is_ok_and(|v| matches!(v.as_str(), "0" | "false" | "off"));
    let dnt = std::env::var("DO_NOT_TRACK").is_ok_and(|v| v == "1" || v == "true");
    !KEY.is_empty() && !env_off("PINE_TELEMETRY") && !dnt && !off_marker().is_some_and(|m| m.exists())
}

/// GitHub login when logged in, else this machine's random id.
fn distinct_id() -> String {
    if let Some(a) = crate::team::load_auth() {
        return a.login;
    }
    let Ok(path) = crate::paths::root().map(|r| r.join("telemetry-id")) else { return "anonymous".into() };
    if let Ok(id) = std::fs::read_to_string(&path) {
        if !id.trim().is_empty() {
            return id.trim().to_string();
        }
    }
    let id = format!("anon-{}", ulid::Ulid::new());
    let _ = std::fs::create_dir_all(path.parent().unwrap_or(&path));
    let _ = std::fs::write(&path, &id);
    id
}

/// An event on its way; `wait()` gives it up to a second to finish.
pub struct Pending(Option<mpsc::Receiver<()>>);

impl Pending {
    pub fn wait(self) {
        if let Some(rx) = self.0 {
            let _ = rx.recv_timeout(Duration::from_secs(1));
        }
    }
}

/// Send one event in the background.
pub fn send(event: &str, props: Value) -> Pending {
    if !enabled() {
        return Pending(None);
    }
    let mut properties = json!({
        "$lib": "pine-cli",
        "version": crate::PINE_VERSION,
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "logged_in": crate::team::load_auth().is_some(),
    });
    if let (Some(p), Value::Object(extra)) = (properties.as_object_mut(), props) {
        p.extend(extra);
    }
    let body = json!({ "api_key": KEY, "event": event, "distinct_id": distinct_id(), "properties": properties });
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build();
        if let Ok(rt) = rt {
            rt.block_on(async {
                if let Ok(c) = reqwest::Client::builder().timeout(Duration::from_secs(3)).build() {
                    let _ = c.post(format!("{HOST}/i/v0/e/")).json(&body).send().await;
                }
            });
        }
        let _ = tx.send(());
    });
    Pending(Some(rx))
}

/// `pn telemetry [on|off]`.
pub fn set(state: Option<&str>) -> Result<()> {
    let Some(m) = off_marker() else { anyhow::bail!("can't find ~/.pine") };
    match state {
        Some("off") => {
            std::fs::create_dir_all(m.parent().unwrap())?;
            std::fs::write(&m, b"")?;
            println!("Usage events are off.");
        }
        Some("on") => {
            let _ = std::fs::remove_file(&m);
            println!("Usage events are on: pn sends which command ran (never its arguments or your code).");
        }
        None => println!(
            "Usage events are {}. They say which pn command ran, never its arguments, paths or code. `pn telemetry off` to stop.",
            if enabled() { "on" } else { "off" }
        ),
        Some(other) => anyhow::bail!("pn telemetry takes on or off, not {other}"),
    }
    Ok(())
}
