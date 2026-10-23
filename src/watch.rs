//! `pn watch`: keep syncing in the background, no terminal needed.
//!
//! - `pn watch`            run the watcher here (what the background job runs)
//! - `pn watch --install`  start it now and at every login (launchd on macOS,
//!                         a systemd user unit on Linux); restarted if it dies
//! - `pn watch --uninstall`, `pn watch status`
//!
//! It covers the same sessions as `pn sync`: pn's recordings plus Claude
//! Code's and Codex's own files (plain CLI and desktop apps), since login.
//! The log is `~/.pine/watch.log`.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{bail, Context, Result};

const LABEL: &str = "dev.usepine.watch";

fn home() -> Result<PathBuf> {
    dirs::home_dir().context("no home dir")
}

fn log_path() -> Result<PathBuf> {
    Ok(crate::paths::root()?.join("watch.log"))
}

fn plist_path() -> Result<PathBuf> {
    Ok(home()?.join(format!("Library/LaunchAgents/{LABEL}.plist")))
}

fn unit_path() -> Result<PathBuf> {
    Ok(home()?.join(".config/systemd/user/pine-watch.service"))
}

fn run(cmd: &mut Command) -> bool {
    cmd.output().map(|o| o.status.success()).unwrap_or(false)
}

fn uid() -> String {
    Command::new("id").arg("-u").output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default()
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

pub fn install() -> Result<()> {
    let exe = std::env::current_exe()?.canonicalize()?;
    let log = log_path()?;
    std::fs::create_dir_all(log.parent().unwrap())?;
    if crate::team::load_auth().is_none() {
        println!("Note: you're not logged in, so it will wait until you run `pn login`.");
    }
    if cfg!(target_os = "macos") {
        let plist = plist_path()?;
        std::fs::create_dir_all(plist.parent().unwrap())?;
        std::fs::write(
            &plist,
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key><array><string>{}</string><string>watch</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ProcessType</key><string>Background</string>
  <key>StandardOutPath</key><string>{}</string>
  <key>StandardErrorPath</key><string>{}</string>
</dict>
</plist>
"#,
                xml(&exe.to_string_lossy()),
                xml(&log.to_string_lossy()),
                xml(&log.to_string_lossy()),
            ),
        )?;
        let domain = format!("gui/{}", uid());
        run(Command::new("launchctl").args(["bootout", &format!("{domain}/{LABEL}")])); // replace an older one
        if !run(Command::new("launchctl").args(["bootstrap", &domain]).arg(&plist)) {
            bail!("launchctl couldn't start it; try: launchctl bootstrap {domain} {}", plist.display());
        }
    } else if cfg!(target_os = "linux") {
        let unit = unit_path()?;
        std::fs::create_dir_all(unit.parent().unwrap())?;
        std::fs::write(
            &unit,
            format!(
                "[Unit]\nDescription=Pine: sync AI sessions to app.usepine.dev\n\n[Service]\nExecStart={} watch\nRestart=always\nRestartSec=10\nStandardOutput=append:{}\nStandardError=append:{}\n\n[Install]\nWantedBy=default.target\n",
                exe.display(),
                log.display(),
                log.display()
            ),
        )?;
        run(Command::new("systemctl").args(["--user", "daemon-reload"]));
        if !run(Command::new("systemctl").args(["--user", "enable", "--now", "pine-watch.service"])) {
            bail!("systemctl couldn't start it; try: systemctl --user enable --now pine-watch.service");
        }
    } else {
        bail!("pn watch --install supports macOS and Linux; run `pn watch` in a terminal instead");
    }
    println!("✓ Pine is syncing in the background, now and every time you log in.");
    println!("  Covers: sessions run with pn, plus Claude Code and Codex on their own (CLI and desktop apps).");
    println!("  Log:    tail -f {}", log.display());
    println!("  Stop:   pn watch --uninstall");
    Ok(())
}

pub fn uninstall() -> Result<()> {
    if cfg!(target_os = "macos") {
        run(Command::new("launchctl").args(["bootout", &format!("gui/{}/{LABEL}", uid())]));
        let _ = std::fs::remove_file(plist_path()?);
    } else {
        run(Command::new("systemctl").args(["--user", "disable", "--now", "pine-watch.service"]));
        let _ = std::fs::remove_file(unit_path()?);
    }
    println!("✓ Background sync stopped. `pn sync` still works by hand.");
    Ok(())
}

/// Is the background job running?
pub fn running() -> bool {
    if cfg!(target_os = "macos") {
        Command::new("launchctl")
            .args(["print", &format!("gui/{}/{LABEL}", uid())])
            .output()
            .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).contains("state = running"))
            .unwrap_or(false)
    } else {
        run(Command::new("systemctl").args(["--user", "is-active", "--quiet", "pine-watch.service"]))
    }
}

pub fn installed() -> bool {
    if cfg!(target_os = "macos") { plist_path() } else { unit_path() }.map(|p| p.exists()).unwrap_or(false)
}

pub fn status() -> Result<()> {
    match (installed(), running()) {
        (true, true) => println!("Background sync is running."),
        (true, false) => println!("Background sync is installed but not running. Reinstall: pn watch --install"),
        _ => println!("Background sync is off. Turn it on: pn watch --install"),
    }
    let log = log_path()?;
    if let Ok(text) = std::fs::read_to_string(&log) {
        let lines: Vec<_> = text.lines().collect();
        println!("Last lines of {}:", log.display());
        for l in &lines[lines.len().saturating_sub(8)..] {
            println!("  {l}");
        }
    }
    Ok(())
}

/// After `pn update` swaps the binary, restart the job so it runs the new one.
pub fn restart_if_installed() {
    if !installed() {
        return;
    }
    if cfg!(target_os = "macos") {
        run(Command::new("launchctl").args(["kickstart", "-k", &format!("gui/{}/{LABEL}", uid())]));
    } else {
        run(Command::new("systemctl").args(["--user", "restart", "pine-watch.service"]));
    }
}
