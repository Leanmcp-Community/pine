//! Spawn `claude` (or any command) with the env vars that route its HTTPS
//! traffic through our local MITM proxy.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{anyhow, Context, Result};
use tokio::process::{Child, Command};

use crate::paths;

#[derive(Debug, Clone)]
pub struct LaunchSpec {
    pub claude_path: PathBuf,
    pub args: Vec<OsString>,
    pub proxy_url: String,
    pub session_id: String,
}

/// Resolve the Windsurf binary path: explicit override, then `which windsurf`,
/// then platform default install locations.
pub fn resolve_windsurf(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        if p.exists() {
            return Ok(p.to_path_buf());
        }
        return Err(anyhow!("--windsurf-bin {} does not exist", p.display()));
    }

    // On macOS, prefer the real Electron binary inside Windsurf.app over
    // whatever `which windsurf` finds — the latter is typically the
    // ~/.codeium/windsurf/bin/windsurf CLI shim, which uses `open` to launch
    // the GUI as a detached process. That detaches the child from pine,
    // so the supervisor sees an immediate exit(0) and tears down the proxy
    // before any real traffic flows.
    #[cfg(target_os = "macos")]
    {
        for c in [
            "/Applications/Windsurf.app/Contents/MacOS/Electron",
            "/Applications/Windsurf.app/Contents/MacOS/Windsurf",
        ] {
            let p = PathBuf::from(c);
            if p.is_file() {
                return Ok(p);
            }
        }
    }

    if let Some(p) = which("windsurf") {
        return Ok(p);
    }

    let candidates: &[&str] = if cfg!(target_os = "macos") {
        &[
            "/Applications/Windsurf.app/Contents/MacOS/Electron",
            "/Applications/Windsurf.app/Contents/MacOS/Windsurf",
        ]
    } else if cfg!(target_os = "linux") {
        &[
            "/usr/share/windsurf/windsurf",
            "/opt/windsurf/windsurf",
            "/usr/bin/windsurf",
        ]
    } else {
        &[]
    };
    for c in candidates {
        let p = PathBuf::from(c);
        if p.is_file() {
            return Ok(p);
        }
    }

    Err(anyhow!(
        "could not locate Windsurf; pass --windsurf-bin /path/to/windsurf"
    ))
}

/// Resolve the Antigravity **desktop GUI** (Electron) binary — as opposed to
/// the `agy` CLI that `resolve_antigravity` finds. On macOS this is the Electron
/// executable inside `/Applications/Antigravity.app`; the GUI is Chromium-based
/// (it ships an Electron Framework + Helper apps), so it needs the same
/// `--proxy-server` launch treatment as Windsurf, not the env-var proxy the CLI
/// uses. Pass an explicit path to override.
pub fn resolve_antigravity_gui(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        if p.exists() {
            return Ok(p.to_path_buf());
        }
        return Err(anyhow!("--gui-bin {} does not exist", p.display()));
    }

    let candidates: &[&str] = if cfg!(target_os = "macos") {
        &[
            "/Applications/Antigravity.app/Contents/MacOS/Antigravity",
            "/Applications/Antigravity.app/Contents/MacOS/Electron",
        ]
    } else if cfg!(target_os = "linux") {
        &[
            "/usr/share/antigravity/antigravity",
            "/opt/antigravity/antigravity",
            "/usr/bin/antigravity",
        ]
    } else {
        &[]
    };
    for c in candidates {
        let p = PathBuf::from(c);
        if p.is_file() {
            return Ok(p);
        }
    }

    // Also cover a per-user install under ~/Applications on macOS.
    #[cfg(target_os = "macos")]
    if let Some(home) = dirs::home_dir() {
        let p = home.join("Applications/Antigravity.app/Contents/MacOS/Antigravity");
        if p.is_file() {
            return Ok(p);
        }
    }

    Err(anyhow!(
        "could not locate the Antigravity desktop app; pass --gui-bin /path/to/Antigravity \
         (the Electron binary inside Antigravity.app/Contents/MacOS/)"
    ))
}

/// Best-effort: check whether Windsurf is already running. If it is, a second
/// launch will silently attach to the existing process and our env is dropped.
pub fn is_windsurf_running() -> bool {
    !running_windsurf_pids().is_empty()
}

/// Best-effort: check whether the Antigravity desktop GUI is already running.
/// Like Windsurf, a second Electron launch hands its args to the existing
/// instance via the single-instance lock and drops our proxy/CA env.
pub fn is_antigravity_gui_running() -> bool {
    !running_antigravity_pids().is_empty()
}

/// Return PIDs of every running Antigravity **desktop GUI** process (main +
/// Electron helpers). Empty if none. Does NOT match the `agy` CLI — that binary
/// is named `agy`, not `antigravity`.
pub fn running_antigravity_pids() -> Vec<u32> {
    use std::process::Command;
    let mut pids: Vec<u32> = Vec::new();
    let push_pgrep = |args: &[&str], out: &mut Vec<u32>| {
        let bin = if cfg!(target_os = "macos") {
            "/usr/bin/pgrep"
        } else {
            "pgrep"
        };
        if let Ok(o) = Command::new(bin).args(args).output() {
            if o.status.success() {
                for line in String::from_utf8_lossy(&o.stdout).lines() {
                    if let Ok(p) = line.trim().parse::<u32>() {
                        if !out.contains(&p) {
                            out.push(p);
                        }
                    }
                }
            }
        }
    };

    #[cfg(target_os = "macos")]
    {
        push_pgrep(&["-f", "Antigravity.app/Contents/"], &mut pids);
        push_pgrep(&["-i", "-f", "Antigravity Helper"], &mut pids);
        push_pgrep(&["-x", "Antigravity"], &mut pids);
    }
    #[cfg(target_os = "linux")]
    {
        push_pgrep(&["-x", "antigravity"], &mut pids);
        push_pgrep(&["-f", "antigravity"], &mut pids);
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = push_pgrep;
    }

    let me = std::process::id();
    pids.retain(|&p| p != me);
    pids
}

/// Terminate every running Antigravity desktop GUI process, using the same
/// SIGTERM → SIGSTOP+SIGKILL strategy as [`kill_running_windsurf`].
pub async fn kill_running_antigravity(grace: std::time::Duration) -> usize {
    kill_running_electron(grace, running_antigravity_pids).await
}

/// Return PIDs of every running Windsurf-related process (main + helpers).
/// Empty if none. Best-effort, cross-platform.
pub fn running_windsurf_pids() -> Vec<u32> {
    use std::process::Command;
    let mut pids: Vec<u32> = Vec::new();
    let push_pgrep = |args: &[&str], out: &mut Vec<u32>| {
        let bin = if cfg!(target_os = "macos") {
            "/usr/bin/pgrep"
        } else {
            "pgrep"
        };
        if let Ok(o) = Command::new(bin).args(args).output() {
            if o.status.success() {
                for line in String::from_utf8_lossy(&o.stdout).lines() {
                    if let Ok(p) = line.trim().parse::<u32>() {
                        if !out.contains(&p) {
                            out.push(p);
                        }
                    }
                }
            }
        }
    };

    #[cfg(target_os = "macos")]
    {
        // Main app binary, Electron renderer, and helper processes all live
        // under Windsurf.app. `-f` matches against the full argv path so we
        // catch every Helper (Renderer/GPU/Plugin) too.
        push_pgrep(&["-f", "Windsurf.app/Contents/"], &mut pids);
        push_pgrep(&["-i", "-f", "Windsurf Helper"], &mut pids);
        push_pgrep(&["-x", "Windsurf"], &mut pids);
    }
    #[cfg(target_os = "linux")]
    {
        push_pgrep(&["-x", "windsurf"], &mut pids);
        push_pgrep(&["-f", "windsurf"], &mut pids);
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = push_pgrep;
    }

    // Never list ourselves — pgrep -f might match the pine process if
    // "windsurf" appears in our own argv (it does: `pine windsurf`).
    let me = std::process::id();
    pids.retain(|&p| p != me);
    pids
}

/// Terminate every running Windsurf process. Strategy:
///
/// 1. SIGTERM all detected PIDs and wait briefly for graceful exit.
/// 2. For everything still alive, SIGSTOP them first (freezes them so they
///    can't fork new children — Electron's main process otherwise respawns
///    helpers during shutdown faster than we can kill them), then SIGKILL.
/// 3. Loop the STOP+KILL pass until either everything is gone or we hit a
///    safety cap (in case some watchdog keeps respawning the app).
///
/// `_grace` is the upper bound on phase 1 (graceful TERM). Total runtime is
/// capped at roughly `grace + 3s`. Returns the *unique* number of PIDs we
/// signalled across all passes.
pub async fn kill_running_windsurf(grace: std::time::Duration) -> usize {
    kill_running_electron(grace, running_windsurf_pids).await
}

/// Generic Electron-process killer shared by the Windsurf and Antigravity GUI
/// paths. `list` enumerates the live PIDs for the target app on each pass; the
/// SIGTERM → SIGSTOP+SIGKILL loop is identical regardless of which app it is.
async fn kill_running_electron(
    grace: std::time::Duration,
    list: impl Fn() -> Vec<u32>,
) -> usize {
    let initial = list();
    if initial.is_empty() {
        return 0;
    }

    #[cfg(unix)]
    {
        // If pine was launched from Windsurf's own integrated terminal,
        // killing Windsurf will close our controlling tty and deliver SIGHUP
        // to us. Ignore so we survive long enough to relaunch. SIGPIPE for
        // the same reason — writes to the dead stdout would otherwise kill us.
        unsafe {
            libc::signal(libc::SIGHUP, libc::SIG_IGN);
            libc::signal(libc::SIGPIPE, libc::SIG_IGN);
        }
    }

    let mut seen: std::collections::HashSet<u32> = initial.iter().copied().collect();

    // Phase 1: SIGTERM and short graceful wait.
    #[cfg(unix)]
    for &pid in &initial {
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
        }
    }
    let poll = std::time::Duration::from_millis(150);
    let deadline = std::time::Instant::now() + grace;
    while std::time::Instant::now() < deadline {
        if list().is_empty() {
            return seen.len();
        }
        tokio::time::sleep(poll).await;
    }

    // Phase 2: SIGSTOP+SIGKILL passes. Loop because Electron's main process
    // re-forks helpers during shutdown; the only reliable way to drain is to
    // freeze each batch before killing it, then look for any NEW children
    // that appeared.
    let max_passes = 8;
    for _ in 0..max_passes {
        let pids = list();
        if pids.is_empty() {
            break;
        }
        for p in &pids {
            seen.insert(*p);
        }
        #[cfg(unix)]
        {
            // Freeze first — a stopped process can't fork or hand off to a
            // helper. Then kill while frozen.
            for &pid in &pids {
                unsafe {
                    libc::kill(pid as libc::pid_t, libc::SIGSTOP);
                }
            }
            for &pid in &pids {
                unsafe {
                    libc::kill(pid as libc::pid_t, libc::SIGKILL);
                }
            }
            // SIGCONT so the kernel can deliver the SIGKILL to a stopped
            // process and reap it. (On Linux SIGKILL on a stopped task does
            // get delivered, but CONT removes the race on macOS.)
            for &pid in &pids {
                unsafe {
                    libc::kill(pid as libc::pid_t, libc::SIGCONT);
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }

    seen.len()
}

/// Spawn a Chromium/Electron app (Windsurf or the Antigravity desktop GUI)
/// under the proxy. Unlike a plain Go/Node CLI, Chromium ignores `HTTP_PROXY`
/// env vars, so the proxy has to be wired in with the `--proxy-server` switch;
/// we still set the env vars too for any child Node tooling the app shells out
/// to. The default-open-cwd behavior mirrors `windsurf .` and is equally
/// sensible for the Antigravity IDE.
pub async fn spawn_electron(spec: LaunchSpec, user_data_dir: Option<&Path>) -> Result<Child> {
    let ca_cert = paths::ca_cert_path()?;
    let ca_cert_str = ca_cert
        .to_str()
        .ok_or_else(|| anyhow!("CA cert path is not utf-8: {}", ca_cert.display()))?
        .to_string();

    let mut cmd = Command::new(&spec.claude_path);

    // Chromium switches go BEFORE user-supplied args.
    cmd.arg(format!("--proxy-server={}", spec.proxy_url));
    cmd.arg("--proxy-bypass-list=<-loopback>;localhost;127.0.0.1;::1");
    if let Some(d) = user_data_dir {
        cmd.arg(format!("--user-data-dir={}", d.display()));
    }
    cmd.args(&spec.args);

    // If the user didn't pass a positional path after `--`, default to opening
    // the directory `pine windsurf` was invoked from. Without this, the
    // bare Electron binary reopens whatever workspace it had last — which is
    // emphatically NOT what someone running `pine windsurf` from a
    // project dir expects (`windsurf .` is the equivalent shim behavior).
    let user_supplied_path = spec.args.iter().any(|a| {
        let s = a.to_string_lossy();
        !s.is_empty() && !s.starts_with('-')
    });
    if !user_supplied_path {
        if let Ok(cwd) = std::env::current_dir() {
            cmd.arg(cwd);
        }
    }

    cmd.env("HTTPS_PROXY", &spec.proxy_url);
    cmd.env("https_proxy", &spec.proxy_url);
    cmd.env("HTTP_PROXY", &spec.proxy_url);
    cmd.env("http_proxy", &spec.proxy_url);
    cmd.env("ALL_PROXY", &spec.proxy_url);
    cmd.env("all_proxy", &spec.proxy_url);
    cmd.env("NO_PROXY", "localhost,127.0.0.1,::1");
    cmd.env("no_proxy", "localhost,127.0.0.1,::1");

    cmd.env("NODE_EXTRA_CA_CERTS", &ca_cert_str);
    cmd.env("SSL_CERT_FILE", &ca_cert_str);
    cmd.env("REQUESTS_CA_BUNDLE", &ca_cert_str);
    cmd.env("GRPC_DEFAULT_SSL_ROOTS_FILE_PATH", &ca_cert_str);

    cmd.env("PINE_SESSION", &spec.session_id);

    cmd.stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    // New process group so we can SIGKILL the whole Electron tree (zygote +
    // GPU + utility + renderers) on force-quit, not just the parent PID.
    #[cfg(unix)]
    {
        #[allow(unused_imports)]
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                if libc::setpgid(0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    cmd.spawn()
        .with_context(|| format!("spawning {}", spec.claude_path.display()))
}

/// Resolve the `claude` binary path: explicit override, then `$PATH`, then
/// the `.exe` shim that npm's @anthropic-ai/claude-code installs on macOS.
pub fn resolve_claude(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        if p.exists() {
            return Ok(p.to_path_buf());
        }
        return Err(anyhow!("--claude-bin {} does not exist", p.display()));
    }

    if let Some(p) = which("claude") {
        return Ok(p);
    }
    if let Some(p) = which("claude.exe") {
        return Ok(p);
    }
    Err(anyhow!(
        "could not find `claude` on $PATH; pass --claude-bin /path/to/claude"
    ))
}

/// Resolve the `codex` binary path: explicit override, then `$PATH`, then the
/// `.exe` shim name some package managers expose on Windows-like setups.
pub fn resolve_codex(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        if p.exists() {
            return Ok(p.to_path_buf());
        }
        return Err(anyhow!("--codex-bin {} does not exist", p.display()));
    }

    if let Some(p) = which("codex") {
        return Ok(p);
    }
    if let Some(p) = which("codex.exe") {
        return Ok(p);
    }
    Err(anyhow!(
        "could not find `codex` on $PATH; pass --codex-bin /path/to/codex"
    ))
}

/// Resolve the Antigravity CLI (`agy`) binary path: explicit override, then
/// `$PATH`, then the common install locations. `agy` is a self-contained Go
/// binary (Google's Antigravity agent CLI), not the Electron GUI under
/// `/Applications/Antigravity.app`.
pub fn resolve_antigravity(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        if p.exists() {
            return Ok(p.to_path_buf());
        }
        return Err(anyhow!("--agy-bin {} does not exist", p.display()));
    }

    if let Some(p) = which("agy") {
        return Ok(p);
    }

    // The installer drops `agy` in ~/.local/bin by default; cover a couple of
    // other common spots before giving up.
    if let Some(home) = dirs::home_dir() {
        for rel in [".local/bin/agy", "bin/agy"] {
            let p = home.join(rel);
            if p.is_file() {
                return Ok(p);
            }
        }
    }
    for c in ["/usr/local/bin/agy", "/opt/homebrew/bin/agy"] {
        let p = PathBuf::from(c);
        if p.is_file() {
            return Ok(p);
        }
    }

    Err(anyhow!(
        "could not find `agy` on $PATH; pass --agy-bin /path/to/agy"
    ))
}

/// Spawn the Antigravity CLI (`agy`) under the proxy.
///
/// `agy` is a Go binary, so `net/http` picks up `HTTP(S)_PROXY`/`NO_PROXY`
/// from the environment automatically — no Chromium `--proxy-server` flags
/// needed (those only apply to the Electron GUI). On macOS Go validates TLS
/// against the system keychain, so the pine root CA must be OS-trusted
/// (`pine ca trust`); the `*_CA_*` env vars below are best-effort for the
/// Node/Electron tooling `agy` may shell out to.
pub async fn spawn_antigravity(spec: LaunchSpec) -> Result<Child> {
    let ca_cert = paths::ca_cert_path()?;
    let ca_cert_str = ca_cert
        .to_str()
        .ok_or_else(|| anyhow!("CA cert path is not utf-8: {}", ca_cert.display()))?
        .to_string();

    let mut cmd = Command::new(&spec.claude_path);
    cmd.args(&spec.args);

    cmd.env("HTTPS_PROXY", &spec.proxy_url);
    cmd.env("https_proxy", &spec.proxy_url);
    cmd.env("HTTP_PROXY", &spec.proxy_url);
    cmd.env("http_proxy", &spec.proxy_url);
    cmd.env("ALL_PROXY", &spec.proxy_url);
    cmd.env("all_proxy", &spec.proxy_url);
    cmd.env("NO_PROXY", "localhost,127.0.0.1,::1");
    cmd.env("no_proxy", "localhost,127.0.0.1,::1");

    cmd.env("NODE_EXTRA_CA_CERTS", &ca_cert_str);
    cmd.env("SSL_CERT_FILE", &ca_cert_str);
    cmd.env("BUN_CA_BUNDLE", &ca_cert_str);
    cmd.env("REQUESTS_CA_BUNDLE", &ca_cert_str);
    cmd.env("GRPC_DEFAULT_SSL_ROOTS_FILE_PATH", &ca_cert_str);

    cmd.env("PINE_SESSION", &spec.session_id);

    cmd.stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    // New process group so we can signal the whole tree on force-quit — `agy`
    // can launch helper processes (and drive the Electron app over CDP).
    #[cfg(unix)]
    {
        #[allow(unused_imports)]
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                if libc::setpgid(0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    cmd.spawn()
        .with_context(|| format!("spawning {}", spec.claude_path.display()))
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

pub async fn spawn(spec: LaunchSpec) -> Result<Child> {
    let ca_cert = paths::ca_cert_path()?;
    let ca_cert_str = ca_cert
        .to_str()
        .ok_or_else(|| anyhow!("CA cert path is not utf-8: {}", ca_cert.display()))?
        .to_string();

    let mut cmd = Command::new(&spec.claude_path);
    cmd.args(&spec.args);

    // HTTPS / HTTP proxy routing.
    cmd.env("HTTPS_PROXY", &spec.proxy_url);
    cmd.env("https_proxy", &spec.proxy_url);
    cmd.env("HTTP_PROXY", &spec.proxy_url);
    cmd.env("http_proxy", &spec.proxy_url);
    cmd.env("ALL_PROXY", &spec.proxy_url);
    cmd.env("all_proxy", &spec.proxy_url);
    // Don't proxy ourselves recursively.
    cmd.env("NO_PROXY", "localhost,127.0.0.1,::1");
    cmd.env("no_proxy", "localhost,127.0.0.1,::1");

    // Trust our CA. Cover the three common runtimes Claude Code might use.
    cmd.env("NODE_EXTRA_CA_CERTS", &ca_cert_str);
    cmd.env("SSL_CERT_FILE", &ca_cert_str);
    cmd.env("BUN_CA_BUNDLE", &ca_cert_str);
    cmd.env("REQUESTS_CA_BUNDLE", &ca_cert_str);

    cmd.env("PINE_SESSION", &spec.session_id);

    cmd.stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    cmd.spawn()
        .with_context(|| format!("spawning {}", spec.claude_path.display()))
}
