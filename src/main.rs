//! pine entry point: CLI parsing + orchestration.

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use time::OffsetDateTime;
use tracing::warn;
use tracing_subscriber::EnvFilter;
use ulid::Ulid;

mod banner;
mod ca;
mod cloud;
mod launcher;
mod log;
mod paths;
mod proxy;
mod sse;
mod team;
mod update;
mod ws;

/// Version string shown by `pine --version`. Composed of the
/// crate version from Cargo.toml plus the short git hash (with a
/// `-dirty` suffix if the working tree had uncommitted changes at
/// build time). The git hash comes from `build.rs`.
pub(crate) const PINE_VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("PINE_GIT_HASH"),
    ")",
);

const DEFAULT_HOSTS: &[&str] = &[
    "api.anthropic.com",
    "*.anthropic.com",
    "statsig.anthropic.com",
    "console.anthropic.com",
];

const WINDSURF_HOSTS: &[&str] = &[
    // Cognition / Codeium first-party
    "*.codeium.com",
    "*.codeiumdata.com",
    "*.windsurf.com",
    "*.cognition.dev",
    "*.cognition.ai",
    "*.codeium.dev",
    "*.fireworks.ai",
    "server.codeium.com",
    "inference.codeium.com",
    "exa.codeium.com",
    "telemetry.codeium.com",
    // Auth
    "auth.codeium.com",
    "login.windsurf.com",
    // VS Code marketplace (Windsurf typically uses Open VSX)
    "open-vsx.org",
    "*.open-vsx.org",
    "marketplace.visualstudio.com",
    "*.vsassets.io",
    "*.gallerycdn.vsassets.io",
    // Updates / user content
    "update.windsurf.com",
    "*.windsurfusercontent.com",
    // Common upstream model providers Windsurf may forward to
    "api.openai.com",
    "api.anthropic.com",
    "*.anthropic.com",
    "generativelanguage.googleapis.com",
];

// Antigravity CLI (`agy`) talks to Google's Cloud Code / Gemini Code Assist
// backend, Vertex AI, and Google auth — plus Anthropic (Antigravity serves
// Claude models). `*.googleapis.com` is the broad catch-all; the named hosts
// document the ones observed in the `agy` binary.
const ANTIGRAVITY_HOSTS: &[&str] = &[
    // Cloud Code / Gemini Code Assist agent backend (primary LLM traffic)
    "cloudcode-pa.googleapis.com",
    "businessaicode.googleapis.com",
    "aiplatform.googleapis.com",
    "generativelanguage.googleapis.com",
    // Google auth / identity
    "oauth2.googleapis.com",
    "iamcredentials.googleapis.com",
    "iam.googleapis.com",
    "secretmanager.googleapis.com",
    "www.googleapis.com",
    // Broad catch-all for any other Google API surface
    "*.googleapis.com",
    "*.google.com",
    // Antigravity also serves Claude models
    "api.anthropic.com",
    "*.anthropic.com",
];

const CODEX_HOSTS: &[&str] = &[
    "api.openai.com",
    "*.openai.com",
    "chatgpt.com",
    "*.chatgpt.com",
    "auth.openai.com",
    "*.oaistatic.com",
    "*.oaiusercontent.com",
];

#[derive(Parser, Debug)]
#[command(
    name = "pine",
    version = PINE_VERSION,
    about = "Tap AI tool HTTPS traffic into ~/.pine (logs are kept forever).",
    disable_help_subcommand = true
)]
struct Cli {
    /// Subcommand. With no subcommand we launch `claude` under the proxy.
    #[command(subcommand)]
    command: Option<Cmd>,

    /// Path to the `claude` binary. Defaults to `which claude`.
    #[arg(long, value_name = "PATH", global = true)]
    claude_bin: Option<PathBuf>,

    /// Local proxy port. 0 = pick a free one (default).
    #[arg(long, default_value_t = 0u16, global = true)]
    port: u16,

    /// Hostnames or `*.suffix` patterns to MITM. Repeatable, comma-separated.
    #[arg(long, value_delimiter = ',', global = true)]
    hosts: Vec<String>,

    /// Refuse non-tapped hosts entirely instead of blind-tunneling them.
    #[arg(long, default_value_t = false, global = true)]
    passthrough_only_logged: bool,

    /// Disable header redaction (default: redact auth/cookie/api-key).
    #[arg(long, default_value_t = false, global = true)]
    no_redact: bool,

    /// Forwarded to `claude` after `--`.
    #[arg(last = true)]
    claude_args: Vec<OsString>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Log in with GitHub so sessions sync to your team (pine login).
    Login,
    /// Log out of Pine on this device. Recording continues locally.
    Logout,
    /// Who you are, and whether sync is live or local-only.
    Whoami,
    /// Upload sessions now (and keep watching). `status`, `pause`, `resume`.
    Sync {
        #[command(subcommand)]
        sub: Option<team::SyncCmd>,
        /// One pass, then exit.
        #[arg(long, default_value_t = false)]
        once: bool,
        /// Also share sessions recorded before you logged in.
        #[arg(long, default_value_t = false)]
        all: bool,
    },
    /// Name the current conversation at this moment so teammates can pull it.
    Checkpoint {
        /// Checkpoint name, e.g. "rate-limit-working".
        name: String,
        /// What this checkpoint is.
        #[arg(short = 'm', long)]
        note: Option<String>,
        /// Session id (default: the most recent session).
        #[arg(long)]
        session: Option<String>,
    },
    /// List your team's checkpoints.
    Checkpoints,
    /// Download a teammate's checkpoint (by name) or session (by id).
    Pull {
        /// Checkpoint name or session id.
        target: String,
    },
    /// Open a session (default: the most recent) on app.usepine.dev.
    Open {
        /// Session id.
        id: Option<String>,
    },
    /// Configure and run optional cloud uploads.
    Cloud { #[command(subcommand)] sub: cloud::Command },
    /// CA management.
    Ca {
        #[command(subcommand)]
        sub: CaCmd,
    },
    /// Print the resolved `~/.pine` root and exit.
    Where,
    /// Run only the proxy. Doesn't launch claude. Stays up until Ctrl-C.
    ///
    /// Prints the env vars you need to wire any other client (curl, your
    /// own claude invocation, etc.) up to it.
    Proxy,
    /// Launch Windsurf (Cognition's VS Code fork) under the proxy.
    Windsurf {
        /// Path to the Windsurf binary. Defaults to `which windsurf` and
        /// platform install paths.
        #[arg(long, value_name = "PATH")]
        windsurf_bin: Option<PathBuf>,
        /// Run Windsurf with an isolated user data dir (safer for testing —
        /// keeps proxy/CA changes out of your real Windsurf profile).
        #[arg(long, value_name = "DIR")]
        user_data_dir: Option<PathBuf>,
        /// Skip the OS-trust check for the pine root CA. Chromium will
        /// reject our forged certs without it; only useful for debugging.
        #[arg(long, default_value_t = false)]
        skip_trust_check: bool,
        /// Do NOT kill an already-running Windsurf. Default behavior is to
        /// terminate any running Windsurf (SIGTERM, then SIGKILL after a
        /// short grace period) and relaunch it under the proxy, because
        /// Electron's single-instance lock would otherwise silently forward
        /// the new launch to the existing process and drop our proxy / CA
        /// env. Pass this flag to abort with an error instead.
        #[arg(long, default_value_t = false)]
        no_restart: bool,
        /// Forwarded to Windsurf after `--`.
        #[arg(last = true)]
        windsurf_args: Vec<OsString>,
    },
    /// Launch Antigravity under the proxy. By default this runs the `agy`
    /// terminal CLI (Google's agent CLI). Pass `--gui` to instead launch the
    /// Antigravity desktop app (the Electron IDE under /Applications).
    #[command(alias = "agy")]
    Antigravity {
        /// Path to the `agy` binary. Defaults to `which agy` and the common
        /// install locations (~/.local/bin/agy, /usr/local/bin/agy, ...).
        #[arg(long, value_name = "PATH")]
        agy_bin: Option<PathBuf>,
        /// Launch the Antigravity **desktop GUI** (the Electron IDE) under the
        /// proxy instead of the `agy` terminal CLI. The GUI is Chromium-based,
        /// so it gets the same `--proxy-server` + CA treatment as Windsurf.
        #[arg(long, default_value_t = false)]
        gui: bool,
        /// (GUI only) Path to the Antigravity desktop app's Electron binary.
        /// Defaults to /Applications/Antigravity.app/Contents/MacOS/Antigravity.
        #[arg(long, value_name = "PATH")]
        gui_bin: Option<PathBuf>,
        /// (GUI only) Run the GUI with an isolated user-data dir — keeps the
        /// proxy/CA-tapped launch out of your real Antigravity profile and
        /// sidesteps Electron's single-instance lock.
        #[arg(long, value_name = "DIR")]
        user_data_dir: Option<PathBuf>,
        /// (GUI only) Skip the OS-trust check for the pine root CA.
        /// Chromium rejects our forged certs without it; debugging only.
        #[arg(long, default_value_t = false)]
        skip_trust_check: bool,
        /// (GUI only) Do NOT kill an already-running Antigravity GUI. Default
        /// is to terminate it and relaunch under the proxy, because Electron's
        /// single-instance lock would otherwise forward the launch to the
        /// existing process and drop our proxy/CA env.
        #[arg(long, default_value_t = false)]
        no_restart: bool,
        /// Forwarded to `agy` (CLI) or the Antigravity GUI after `--`.
        #[arg(last = true)]
        agy_args: Vec<OsString>,
    },
    /// Launch OpenAI Codex under the proxy.
    Codex {
        /// Path to the `codex` binary. Defaults to `which codex`.
        #[arg(long, value_name = "PATH")]
        codex_bin: Option<PathBuf>,
        /// Forwarded to `codex` after `--`.
        #[arg(last = true)]
        codex_args: Vec<OsString>,
    },
    #[command(hide = true)]
    InstallTelemetry,
    /// Download and install the latest release for this platform, replacing
    /// the running binary in place. No-op if already on the latest commit.
    Update,
}

#[derive(Subcommand, Debug)]
enum CaCmd {
    /// Print the path to the CA certificate.
    Path,
    /// Print the CA certificate to stdout.
    Print,
    /// Install the pine root CA into the OS trust store (login keychain
    /// on macOS). Required for Chromium-based clients (Windsurf, Cursor).
    Trust,
    /// Remove the pine root CA from the OS trust store.
    Untrust,
    /// Untrust AND delete the on-disk CA files (`~/.pine/ca/`). Next run
    /// will mint a brand-new root CA — you'll need to `ca trust` it again.
    Reset,
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building tokio runtime")
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing();
    install_default_crypto_provider()?;

    match cli.command {
        Some(Cmd::Cloud { sub }) => tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(cloud::run(sub)),
        Some(Cmd::Login) => runtime()?.block_on(team::login()),
        Some(Cmd::Logout) => runtime()?.block_on(team::logout()),
        Some(Cmd::Whoami) => runtime()?.block_on(team::whoami()),
        Some(Cmd::Sync { sub, once, all }) => runtime()?.block_on(team::sync_cmd(sub, once, all)),
        Some(Cmd::Checkpoint { name, note, session }) => runtime()?.block_on(team::checkpoint(name, note, session)),
        Some(Cmd::Checkpoints) => runtime()?.block_on(team::checkpoints()),
        Some(Cmd::Pull { target }) => runtime()?.block_on(team::pull(target)),
        Some(Cmd::Open { id }) => team::open(id),
        Some(Cmd::Where) => {
            println!("{}", paths::root()?.display());
            Ok(())
        }
        Some(Cmd::Ca { sub: CaCmd::Path }) => {
            let _ = ca::Ca::load_or_generate()?;
            println!("{}", paths::ca_cert_path()?.display());
            Ok(())
        }
        Some(Cmd::Ca { sub: CaCmd::Print }) => {
            let ca = ca::Ca::load_or_generate()?;
            print!("{}", ca.cert_pem);
            Ok(())
        }
        Some(Cmd::Ca { sub: CaCmd::Trust }) => ca::os_trust_install(),
        Some(Cmd::Ca { sub: CaCmd::Untrust }) => ca::os_trust_remove(),
        Some(Cmd::Ca { sub: CaCmd::Reset }) => ca::reset(),
        Some(Cmd::Windsurf { .. }) => {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .context("building tokio runtime")?;
            rt.block_on(team::with_live_sync(run_windsurf(cli)))
        }
        Some(Cmd::Antigravity { gui, .. }) => {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .context("building tokio runtime")?;
            if gui {
                rt.block_on(team::with_live_sync(run_antigravity_gui(cli)))
            } else {
                rt.block_on(team::with_live_sync(run_antigravity(cli)))
            }
        }
        Some(Cmd::Codex { .. }) => {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .context("building tokio runtime")?;
            rt.block_on(team::with_live_sync(run_codex(cli)))
        }
        Some(Cmd::Proxy) => {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .context("building tokio runtime")?;
            rt.block_on(team::with_live_sync(run_proxy_only(cli)))
        }
        Some(Cmd::InstallTelemetry) => {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .context("building tokio runtime")?;
            rt.block_on(run_install_telemetry())
        }
        Some(Cmd::Update) => {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .context("building tokio runtime")?;
            rt.block_on(update::run())
        }
        None => {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .context("building tokio runtime")?;
            rt.block_on(team::with_live_sync(run_proxy_and_launch(cli)))
        }
    }
}

async fn run_install_telemetry() -> Result<()> {
    let posthog_key = env!("POSTHOG_API_KEY");
    if posthog_key.is_empty() {
        return Ok(());
    }

    let event_name = format!("{}_install", env!("CARGO_PKG_NAME"));
    let distinct_id = std::env::var("USER")
        .ok()
        .or_else(|| std::env::var("USERNAME").ok())
        .unwrap_or_else(|| "unknown".to_string());

    let client = reqwest::Client::new();
    let payload = serde_json::json!({
        "api_key": posthog_key,
        "event": event_name,
        "properties": {
            "distinct_id": distinct_id,
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "version": env!("CARGO_PKG_VERSION")
        }
    });

    // Fire and forget
    let _ = client.post("https://us.i.posthog.com/capture/")
        .json(&payload)
        .send()
        .await;

    Ok(())
}

fn init_tracing() {
    // Default to warnings only so the banner is the primary signal at
    // startup. Bump with `PINE_LOG=pine=info` (or =debug) when you
    // want per-connection traces.
    let filter = EnvFilter::try_from_env("PINE_LOG")
        .unwrap_or_else(|_| EnvFilter::new("pine=warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .without_time()
        .with_writer(std::io::stderr)
        .try_init();
}

fn install_default_crypto_provider() -> Result<()> {
    // rustls 0.23 requires a process-wide default crypto provider before any
    // ServerConfig / ClientConfig can be built without an explicit provider.
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        rustls::crypto::ring::default_provider()
            .install_default()
            .map_err(|_| anyhow::anyhow!("failed to install rustls ring crypto provider"))?;
    }
    Ok(())
}

async fn run_proxy_and_launch(cli: Cli) -> Result<()> {
    paths::ensure_dir(&paths::root()?)?;
    paths::ensure_dir(&paths::sessions_dir()?)?;
    track_install_event();

    let ca = Arc::new(ca::Ca::load_or_generate()?);

    let session_id = Ulid::new().to_string();
    let logger = log::SessionLogger::create(session_id.clone(), !cli.no_redact).await?;

    let hosts = if cli.hosts.is_empty() {
        DEFAULT_HOSTS.iter().map(|s| s.to_string()).collect()
    } else {
        cli.hosts.clone()
    };
    let host_filter = proxy::HostFilter::new(hosts.clone());

    let listener = proxy::bind_listener(cli.port).await?;
    let local = listener.local_addr()?;
    let proxy_url = format!("http://{}", local);

    let state =
        proxy::build_state(ca.clone(), logger.clone(), host_filter, cli.passthrough_only_logged)
            .await?;

    let claude_path = launcher::resolve_claude(cli.claude_bin.as_deref())?;
    let spec = launcher::LaunchSpec {
        claude_path: claude_path.clone(),
        args: cli.claude_args.clone(),
        proxy_url: proxy_url.clone(),
        session_id: session_id.clone(),
    };

    // Start the proxy task.
    let proxy_state = state.clone();
    let proxy_task = tokio::spawn(async move {
        if let Err(e) = proxy::run(listener, proxy_state).await {
            warn!(error = %e, "proxy loop exited");
        }
    });

    // Print the splash banner BEFORE spawning claude, so it lands at the top
    // of scrollback even if claude paints over the visible area.
    let session_dir = paths::session_dir(&session_id)?;
    let ca_path = paths::ca_cert_path()?;
    let banner_text = banner::Banner::new("pine · launching")
        .row("session", session_id.clone())
        .row("proxy", proxy_url.clone())
        .row("logs", banner::abbrev_path(&session_dir))
        .row("hosts", hosts.join(", "))
        .row("claude", banner::abbrev_path(&claude_path))
        .render();
    print!("{}", banner_text);
    use std::io::Write;
    let _ = std::io::stdout().flush();

    write_last_session_pointer(&session_id, &session_dir, &proxy_url)?;

    // Spawn claude.
    let mut child = launcher::spawn(spec).await?;
    let pid = child.id();

    // Now that we know the child PID, write session metadata.
    let meta = log::SessionMeta {
        session_id: session_id.clone(),
        target: "claude".to_string(),
        started_at: OffsetDateTime::now_utc(),
        ended_at: None,
        claude_argv: cli
            .claude_args
            .iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect(),
        claude_path: claude_path.display().to_string(),
        claude_pid: pid,
        claude_exit_code: None,
        claude_session_id: None,
        claude_version: detect_claude_version(&claude_path),
        proxy_addr: local.to_string(),
        redact: !cli.no_redact,
        host_filter: hosts,
        pine_version: env!("CARGO_PKG_VERSION").to_string(),
        session_dir: Some(session_dir.display().to_string()),
        ca_cert_path: Some(ca_path.display().to_string()),
        cwd: std::env::current_dir().ok().map(|p| p.display().to_string()),
        user: std::env::var("USER").ok().or_else(|| std::env::var("USERNAME").ok()),
        hostname: hostname(),
        os: Some(std::env::consts::OS.to_string()),
        arch: Some(std::env::consts::ARCH.to_string()),
        shell: std::env::var("SHELL").ok(),
        term: std::env::var("TERM").ok(),
        system: Some(collect_system_info()),
    };
    logger.write_meta(&meta).await?;

    // Supervise the child with an escalating signal handler so Ctrl-C
    // ALWAYS gets us out, even when claude's TUI is hung waiting on a
    // dead network connection and ignoring SIGINT.
    let code = supervise_child(&mut child, pid).await;
    if let Err(e) = logger
        .update_meta(|m| {
            m.ended_at = Some(OffsetDateTime::now_utc());
            m.claude_exit_code = code;
        })
        .await
    {
        warn!(error = %e, "updating meta.json on shutdown");
    }
    eprintln!(
        "\npine: claude exited (code={}) — session {}\n           logs at {}",
        code.map(|c| c.to_string()).unwrap_or_else(|| "signal".to_string()),
        session_id,
        session_dir.display()
    );

    // Give the proxy a moment to finalize any in-flight SSE finalization
    // tasks, then drop it. We deliberately do NOT call any "flush all logs"
    // routine here — logs are append-only and persisted as they are written.
    proxy_task.abort();
    let _ = proxy_task.await;

    std::process::exit(code.unwrap_or(0));
}

/// `pine proxy`: stand the proxy up and stay alive until Ctrl-C.
///
/// We deliberately don't fork a child here so the proxy lives as long as
/// the user wants. Useful for manual testing with curl, or for users who
/// want to source the env vars and start `claude` themselves in another
/// terminal.
async fn run_proxy_only(cli: Cli) -> Result<()> {
    paths::ensure_dir(&paths::root()?)?;
    paths::ensure_dir(&paths::sessions_dir()?)?;
    track_install_event();

    let ca = Arc::new(ca::Ca::load_or_generate()?);

    let session_id = Ulid::new().to_string();
    let logger = log::SessionLogger::create(session_id.clone(), !cli.no_redact).await?;

    let hosts = if cli.hosts.is_empty() {
        DEFAULT_HOSTS.iter().map(|s| s.to_string()).collect()
    } else {
        cli.hosts.clone()
    };
    let host_filter = proxy::HostFilter::new(hosts.clone());

    let listener = proxy::bind_listener(cli.port).await?;
    let local = listener.local_addr()?;
    let proxy_url = format!("http://{}", local);

    let state = proxy::build_state(
        ca.clone(),
        logger.clone(),
        host_filter,
        cli.passthrough_only_logged,
    )
    .await?;

    let ca_path = paths::ca_cert_path()?;

    let session_dir = paths::session_dir(&session_id)?;

    // Write session meta now (no claude child to wait on).
    let meta = log::SessionMeta {
        session_id: session_id.clone(),
        target: "proxy".to_string(),
        started_at: OffsetDateTime::now_utc(),
        ended_at: None,
        claude_argv: Vec::new(),
        claude_path: String::new(),
        claude_pid: None,
        claude_exit_code: None,
        claude_session_id: None,
        claude_version: None,
        proxy_addr: local.to_string(),
        redact: !cli.no_redact,
        host_filter: hosts.clone(),
        pine_version: env!("CARGO_PKG_VERSION").to_string(),
        session_dir: Some(session_dir.display().to_string()),
        ca_cert_path: Some(ca_path.display().to_string()),
        cwd: std::env::current_dir().ok().map(|p| p.display().to_string()),
        user: std::env::var("USER").ok().or_else(|| std::env::var("USERNAME").ok()),
        hostname: hostname(),
        os: Some(std::env::consts::OS.to_string()),
        arch: Some(std::env::consts::ARCH.to_string()),
        shell: std::env::var("SHELL").ok(),
        term: std::env::var("TERM").ok(),
        system: Some(collect_system_info()),
    };
    logger.write_meta(&meta).await?;
    let banner_text = banner::Banner::new("pine · proxy (no child)")
        .row("session", session_id.clone())
        .row("proxy", proxy_url.clone())
        .row("logs", banner::abbrev_path(&session_dir))
        .row("traffic", banner::abbrev_path(&session_dir.join("traffic.jsonl")))
        .row("ca", banner::abbrev_path(&ca_path))
        .row("hosts", hosts.join(", "))
        .render();
    print!("{}", banner_text);

    // Copy-pasteable env block under the box.
    println!("# Source these to send another shell's traffic through pine:");
    println!("export HTTPS_PROXY={proxy_url}");
    println!("export HTTP_PROXY={proxy_url}");
    println!("export ALL_PROXY={proxy_url}");
    println!("export NO_PROXY=localhost,127.0.0.1,::1");
    println!("export NODE_EXTRA_CA_CERTS={}", ca_path.display());
    println!("export SSL_CERT_FILE={}", ca_path.display());
    println!("export BUN_CA_BUNDLE={}", ca_path.display());
    println!("# Quick smoke test:");
    println!(
        "# curl --proxy {proxy_url} --cacert {} https://api.anthropic.com/",
        ca_path.display()
    );
    println!("# Ctrl-C to stop.");
    use std::io::Write;
    let _ = std::io::stdout().flush();

    write_last_session_pointer(&session_id, &session_dir, &proxy_url)?;

    // Run the proxy loop until interrupted.
    let proxy_state = state.clone();
    let proxy_task = tokio::spawn(async move {
        if let Err(e) = proxy::run(listener, proxy_state).await {
            warn!(error = %e, "proxy loop exited");
        }
    });

    wait_for_shutdown().await;

    if let Err(e) = logger
        .update_meta(|m| m.ended_at = Some(OffsetDateTime::now_utc()))
        .await
    {
        warn!(error = %e, "updating meta.json on shutdown");
    }
    eprintln!(
        "\npine: proxy stopped — session {}\n           logs at {}",
        session_id,
        session_dir.display()
    );
    proxy_task.abort();
    let _ = proxy_task.await;
    Ok(())
}

async fn run_windsurf(cli: Cli) -> Result<()> {
    let (windsurf_bin, user_data_dir, skip_trust_check, no_restart, windsurf_args) =
        match cli.command {
            Some(Cmd::Windsurf {
                ref windsurf_bin,
                ref user_data_dir,
                skip_trust_check,
                no_restart,
                ref windsurf_args,
            }) => (
                windsurf_bin.clone(),
                user_data_dir.clone(),
                skip_trust_check,
                no_restart,
                windsurf_args.clone(),
            ),
            _ => unreachable!(),
        };
    paths::ensure_dir(&paths::root()?)?;
    paths::ensure_dir(&paths::sessions_dir()?)?;
    track_install_event();

    let ca = Arc::new(ca::Ca::load_or_generate()?);

    // Chromium uses the OS trust store. If our CA isn't there every request
    // will fail with NET::ERR_CERT_AUTHORITY_INVALID. Warn loudly but don't
    // bail — user may be debugging or have trust set up out-of-band.
    if !skip_trust_check {
        match ca::is_os_trusted()? {
            Some(true) => {
                eprintln!("pine: root CA is trusted by the OS keychain ✓");
            }
            Some(false) => {
                eprintln!(
                    "\npine: ⚠  WARNING — root CA is NOT trusted by the OS keychain.\n\
                     \x20            Chromium will reject our forged certs; most Windsurf\n\
                     \x20            requests will fail with NET::ERR_CERT_AUTHORITY_INVALID.\n\
                     \x20            Fix: pine ca trust    (then re-run)\n\
                     \x20            Continuing in 2s — Ctrl-C to abort.\n"
                );
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
            None => {
                eprintln!(
                    "pine: ⚠  cannot verify OS trust on this platform; \
                     ensure the root CA is trusted or Chromium will reject requests."
                );
            }
        }
    }

    if user_data_dir.is_none() && launcher::is_windsurf_running() {
        if no_restart {
            return Err(anyhow::anyhow!(
                "Windsurf is already running and --no-restart was passed. \
                 Quit it first (⌘Q on macOS), drop --no-restart to let \
                 pine kill and relaunch it, or pass --user-data-dir \
                 <fresh> to launch an isolated instance."
            ));
        }
        let before = launcher::running_windsurf_pids();
        eprintln!(
            "pine: Windsurf is already running ({} proc(s)) — terminating it so we can \
             relaunch under the proxy (pass --no-restart to disable)...",
            before.len()
        );
        let killed =
            launcher::kill_running_windsurf(std::time::Duration::from_secs(2)).await;
        // Brief settle delay so the singleton lock file is released.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let survivors = launcher::running_windsurf_pids();
        if !survivors.is_empty() {
            return Err(anyhow::anyhow!(
                "pine: signalled {} Windsurf process(es) but {} are still alive: {:?}.\n\
                 This usually means something (launchctl KeepAlive, a watchdog, or your \
                 desktop session) is respawning Windsurf faster than we can kill it.\n\
                 Try:\n\
                  - quit Windsurf manually (⌘Q), then re-run pine windsurf;\n\
                  - check `launchctl list | grep -i windsurf` for KeepAlive agents;\n\
                  - run with --user-data-dir /tmp/wsf-tap to launch an isolated instance \
                    instead of restarting yours.",
                killed,
                survivors.len(),
                survivors,
            ));
        }
        eprintln!(
            "pine: terminated {} Windsurf process(es); relaunching under the proxy.",
            killed
        );
    }

    let session_id = Ulid::new().to_string();
    let logger = log::SessionLogger::create(session_id.clone(), !cli.no_redact).await?;

    let hosts = if cli.hosts.is_empty() {
        WINDSURF_HOSTS.iter().map(|s| s.to_string()).collect()
    } else {
        cli.hosts.clone()
    };
    let host_filter = proxy::HostFilter::new(hosts.clone());

    let listener = proxy::bind_listener(cli.port).await?;
    let local = listener.local_addr()?;
    let proxy_url = format!("http://{}", local);

    let state =
        proxy::build_state(ca.clone(), logger.clone(), host_filter, cli.passthrough_only_logged)
            .await?;

    let windsurf_path = launcher::resolve_windsurf(windsurf_bin.as_deref())?;
    let spec = launcher::LaunchSpec {
        claude_path: windsurf_path.clone(),
        args: windsurf_args.clone(),
        proxy_url: proxy_url.clone(),
        session_id: session_id.clone(),
    };

    let proxy_state = state.clone();
    let proxy_task = tokio::spawn(async move {
        if let Err(e) = proxy::run(listener, proxy_state).await {
            warn!(error = %e, "proxy loop exited");
        }
    });

    let session_dir = paths::session_dir(&session_id)?;
    let ca_path = paths::ca_cert_path()?;
    let banner_text = banner::Banner::new("pine · windsurf")
        .row("session", session_id.clone())
        .row("proxy", proxy_url.clone())
        .row("logs", banner::abbrev_path(&session_dir))
        .row("hosts", hosts.join(", "))
        .row("windsurf", banner::abbrev_path(&windsurf_path))
        .render();
    print!("{}", banner_text);
    use std::io::Write;
    let _ = std::io::stdout().flush();

    write_last_session_pointer(&session_id, &session_dir, &proxy_url)?;

    let mut child = launcher::spawn_electron(spec, user_data_dir.as_deref()).await?;
    let pid = child.id();
    let spawn_instant = std::time::Instant::now();

    let meta = log::SessionMeta {
        session_id: session_id.clone(),
        target: "windsurf".to_string(),
        started_at: OffsetDateTime::now_utc(),
        ended_at: None,
        claude_argv: windsurf_args
            .iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect(),
        claude_path: windsurf_path.display().to_string(),
        claude_pid: pid,
        claude_exit_code: None,
        claude_session_id: None,
        claude_version: None,
        proxy_addr: local.to_string(),
        redact: !cli.no_redact,
        host_filter: hosts,
        pine_version: env!("CARGO_PKG_VERSION").to_string(),
        session_dir: Some(session_dir.display().to_string()),
        ca_cert_path: Some(ca_path.display().to_string()),
        cwd: std::env::current_dir().ok().map(|p| p.display().to_string()),
        user: std::env::var("USER").ok().or_else(|| std::env::var("USERNAME").ok()),
        hostname: hostname(),
        os: Some(std::env::consts::OS.to_string()),
        arch: Some(std::env::consts::ARCH.to_string()),
        shell: std::env::var("SHELL").ok(),
        term: std::env::var("TERM").ok(),
        system: Some(collect_system_info()),
    };
    logger.write_meta(&meta).await?;

    let code = supervise_child(&mut child, pid).await;
    if let Err(e) = logger
        .update_meta(|m| {
            m.ended_at = Some(OffsetDateTime::now_utc());
            m.claude_exit_code = code;
        })
        .await
    {
        warn!(error = %e, "updating meta.json on shutdown");
    }
    let elapsed = spawn_instant.elapsed();
    eprintln!(
        "\npine: windsurf exited (code={}, after {:.1}s) — session {}\n           logs at {}",
        code.map(|c| c.to_string()).unwrap_or_else(|| "signal".to_string()),
        elapsed.as_secs_f32(),
        session_id,
        session_dir.display()
    );

    // Heuristic: Electron's single-instance lock makes the new process exit
    // ~immediately with code 0 after IPC-handing-off args to an existing
    // Windsurf. If we see that pattern, point the user at the real fix.
    if elapsed < std::time::Duration::from_secs(3) && code == Some(0) {
        eprintln!(
            "\npine: ⚠  windsurf exited almost immediately. Most likely cause:\n\
             \x20            another Windsurf process is alive and the new launch was\n\
             \x20            forwarded to it via Electron's single-instance lock — meaning\n\
             \x20            our proxy and CA settings were dropped.\n\n\
             \x20  This usually means our process-detection missed it. Try:\n\
             \x20    1. Fully kill all Windsurf procs yourself, then re-run:\n\
             \x20         pkill -9 -i windsurf && sleep 1 && pgrep -fl -i windsurf\n\
             \x20         pine windsurf\n\
             \x20    2. Use an isolated profile (bypasses the singleton lock):\n\
             \x20         pine windsurf --user-data-dir /tmp/wsf-tap\n"
        );
    }

    proxy_task.abort();
    let _ = proxy_task.await;

    std::process::exit(code.unwrap_or(0));
}

async fn run_antigravity(cli: Cli) -> Result<()> {
    let (agy_bin, agy_args) = match cli.command {
        Some(Cmd::Antigravity {
            ref agy_bin,
            ref agy_args,
            ..
        }) => (agy_bin.clone(), agy_args.clone()),
        _ => unreachable!(),
    };
    paths::ensure_dir(&paths::root()?)?;
    paths::ensure_dir(&paths::sessions_dir()?)?;
    track_install_event();

    let ca = Arc::new(ca::Ca::load_or_generate()?);

    // `agy` is a Go binary. On macOS Go validates TLS against the system
    // keychain, so without the root CA trusted there every MITM'd request
    // fails. Warn loudly but don't bail — the user may be on a platform where
    // an env-var CA bundle is honored, or have trust set up out-of-band.
    match ca::is_os_trusted()? {
        Some(true) => {
            eprintln!("pine: root CA is trusted by the OS keychain ✓");
        }
        Some(false) => {
            eprintln!(
                "\npine: ⚠  WARNING — root CA is NOT trusted by the OS keychain.\n\
                 \x20            agy is a Go binary and validates TLS against the system\n\
                 \x20            trust store; most requests will fail with a cert error.\n\
                 \x20            Fix: pine ca trust    (then re-run)\n\
                 \x20            Continuing in 2s — Ctrl-C to abort.\n"
            );
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
        None => {
            eprintln!(
                "pine: ⚠  cannot verify OS trust on this platform; \
                 ensure the root CA is trusted or agy will reject requests."
            );
        }
    }

    let session_id = Ulid::new().to_string();
    let logger = log::SessionLogger::create(session_id.clone(), !cli.no_redact).await?;

    let hosts = if cli.hosts.is_empty() {
        ANTIGRAVITY_HOSTS.iter().map(|s| s.to_string()).collect()
    } else {
        cli.hosts.clone()
    };
    let host_filter = proxy::HostFilter::new(hosts.clone());

    let listener = proxy::bind_listener(cli.port).await?;
    let local = listener.local_addr()?;
    let proxy_url = format!("http://{}", local);

    let state =
        proxy::build_state(ca.clone(), logger.clone(), host_filter, cli.passthrough_only_logged)
            .await?;

    let agy_path = launcher::resolve_antigravity(agy_bin.as_deref())?;
    let spec = launcher::LaunchSpec {
        claude_path: agy_path.clone(),
        args: agy_args.clone(),
        proxy_url: proxy_url.clone(),
        session_id: session_id.clone(),
    };

    let proxy_state = state.clone();
    let proxy_task = tokio::spawn(async move {
        if let Err(e) = proxy::run(listener, proxy_state).await {
            warn!(error = %e, "proxy loop exited");
        }
    });

    let session_dir = paths::session_dir(&session_id)?;
    let ca_path = paths::ca_cert_path()?;
    let banner_text = banner::Banner::new("pine · antigravity")
        .row("session", session_id.clone())
        .row("proxy", proxy_url.clone())
        .row("logs", banner::abbrev_path(&session_dir))
        .row("hosts", hosts.join(", "))
        .row("agy", banner::abbrev_path(&agy_path))
        .render();
    print!("{}", banner_text);
    use std::io::Write;
    let _ = std::io::stdout().flush();

    write_last_session_pointer(&session_id, &session_dir, &proxy_url)?;

    let mut child = launcher::spawn_antigravity(spec).await?;
    let pid = child.id();

    let meta = log::SessionMeta {
        session_id: session_id.clone(),
        target: "antigravity".to_string(),
        started_at: OffsetDateTime::now_utc(),
        ended_at: None,
        claude_argv: agy_args
            .iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect(),
        claude_path: agy_path.display().to_string(),
        claude_pid: pid,
        claude_exit_code: None,
        claude_session_id: None,
        claude_version: None,
        proxy_addr: local.to_string(),
        redact: !cli.no_redact,
        host_filter: hosts,
        pine_version: env!("CARGO_PKG_VERSION").to_string(),
        session_dir: Some(session_dir.display().to_string()),
        ca_cert_path: Some(ca_path.display().to_string()),
        cwd: std::env::current_dir().ok().map(|p| p.display().to_string()),
        user: std::env::var("USER").ok().or_else(|| std::env::var("USERNAME").ok()),
        hostname: hostname(),
        os: Some(std::env::consts::OS.to_string()),
        arch: Some(std::env::consts::ARCH.to_string()),
        shell: std::env::var("SHELL").ok(),
        term: std::env::var("TERM").ok(),
        system: Some(collect_system_info()),
    };
    logger.write_meta(&meta).await?;

    let code = supervise_child(&mut child, pid).await;
    if let Err(e) = logger
        .update_meta(|m| {
            m.ended_at = Some(OffsetDateTime::now_utc());
            m.claude_exit_code = code;
        })
        .await
    {
        warn!(error = %e, "updating meta.json on shutdown");
    }
    eprintln!(
        "\npine: agy exited (code={}) — session {}\n           logs at {}",
        code.map(|c| c.to_string()).unwrap_or_else(|| "signal".to_string()),
        session_id,
        session_dir.display()
    );

    proxy_task.abort();
    let _ = proxy_task.await;

    std::process::exit(code.unwrap_or(0));
}

/// Launch the Antigravity **desktop GUI** (the Electron IDE under
/// /Applications) under the proxy. This is the Chromium-based sibling of the
/// `agy` CLI: it ignores `HTTP_PROXY` env vars, so it needs the same
/// `--proxy-server` + OS-trusted-CA + single-instance handling as Windsurf.
/// Mirrors [`run_windsurf`] with Antigravity-specific binary/process helpers.
async fn run_antigravity_gui(cli: Cli) -> Result<()> {
    let (gui_bin, user_data_dir, skip_trust_check, no_restart, gui_args) = match cli.command {
        Some(Cmd::Antigravity {
            ref gui_bin,
            ref user_data_dir,
            skip_trust_check,
            no_restart,
            ref agy_args,
            ..
        }) => (
            gui_bin.clone(),
            user_data_dir.clone(),
            skip_trust_check,
            no_restart,
            agy_args.clone(),
        ),
        _ => unreachable!(),
    };
    paths::ensure_dir(&paths::root()?)?;
    paths::ensure_dir(&paths::sessions_dir()?)?;
    track_install_event();

    let ca = Arc::new(ca::Ca::load_or_generate()?);

    // Chromium uses the OS trust store. If our CA isn't there, every request
    // fails with NET::ERR_CERT_AUTHORITY_INVALID. Warn loudly but don't bail.
    if !skip_trust_check {
        match ca::is_os_trusted()? {
            Some(true) => {
                eprintln!("pine: root CA is trusted by the OS keychain ✓");
            }
            Some(false) => {
                eprintln!(
                    "\npine: ⚠  WARNING — root CA is NOT trusted by the OS keychain.\n\
                     \x20            Chromium will reject our forged certs; most Antigravity\n\
                     \x20            GUI requests will fail with NET::ERR_CERT_AUTHORITY_INVALID.\n\
                     \x20            Fix: pine ca trust    (then re-run)\n\
                     \x20            Continuing in 2s — Ctrl-C to abort.\n"
                );
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
            None => {
                eprintln!(
                    "pine: ⚠  cannot verify OS trust on this platform; \
                     ensure the root CA is trusted or Chromium will reject requests."
                );
            }
        }
    }

    if user_data_dir.is_none() && launcher::is_antigravity_gui_running() {
        if no_restart {
            return Err(anyhow::anyhow!(
                "Antigravity is already running and --no-restart was passed. \
                 Quit it first (⌘Q on macOS), drop --no-restart to let pine \
                 kill and relaunch it, or pass --user-data-dir <fresh> to launch \
                 an isolated instance."
            ));
        }
        let before = launcher::running_antigravity_pids();
        eprintln!(
            "pine: Antigravity GUI is already running ({} proc(s)) — terminating it so we \
             can relaunch under the proxy (pass --no-restart to disable)...",
            before.len()
        );
        let killed = launcher::kill_running_antigravity(std::time::Duration::from_secs(2)).await;
        // Brief settle delay so the singleton lock file is released.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let survivors = launcher::running_antigravity_pids();
        if !survivors.is_empty() {
            return Err(anyhow::anyhow!(
                "pine: signalled {} Antigravity process(es) but {} are still alive: {:?}.\n\
                 Something is respawning it faster than we can kill it. Try:\n\
                  - quit Antigravity manually (⌘Q), then re-run pine agy --gui;\n\
                  - run with --user-data-dir /tmp/agy-tap to launch an isolated instance.",
                killed,
                survivors.len(),
                survivors,
            ));
        }
        eprintln!(
            "pine: terminated {} Antigravity process(es); relaunching under the proxy.",
            killed
        );
    }

    let session_id = Ulid::new().to_string();
    let logger = log::SessionLogger::create(session_id.clone(), !cli.no_redact).await?;

    let hosts = if cli.hosts.is_empty() {
        ANTIGRAVITY_HOSTS.iter().map(|s| s.to_string()).collect()
    } else {
        cli.hosts.clone()
    };
    let host_filter = proxy::HostFilter::new(hosts.clone());

    let listener = proxy::bind_listener(cli.port).await?;
    let local = listener.local_addr()?;
    let proxy_url = format!("http://{}", local);

    let state =
        proxy::build_state(ca.clone(), logger.clone(), host_filter, cli.passthrough_only_logged)
            .await?;

    let gui_path = launcher::resolve_antigravity_gui(gui_bin.as_deref())?;
    let spec = launcher::LaunchSpec {
        claude_path: gui_path.clone(),
        args: gui_args.clone(),
        proxy_url: proxy_url.clone(),
        session_id: session_id.clone(),
    };

    let proxy_state = state.clone();
    let proxy_task = tokio::spawn(async move {
        if let Err(e) = proxy::run(listener, proxy_state).await {
            warn!(error = %e, "proxy loop exited");
        }
    });

    let session_dir = paths::session_dir(&session_id)?;
    let ca_path = paths::ca_cert_path()?;
    let banner_text = banner::Banner::new("pine · antigravity (gui)")
        .row("session", session_id.clone())
        .row("proxy", proxy_url.clone())
        .row("logs", banner::abbrev_path(&session_dir))
        .row("hosts", hosts.join(", "))
        .row("gui", banner::abbrev_path(&gui_path))
        .render();
    print!("{}", banner_text);
    use std::io::Write;
    let _ = std::io::stdout().flush();

    write_last_session_pointer(&session_id, &session_dir, &proxy_url)?;

    let mut child = launcher::spawn_electron(spec, user_data_dir.as_deref()).await?;
    let pid = child.id();
    let spawn_instant = std::time::Instant::now();

    let meta = log::SessionMeta {
        session_id: session_id.clone(),
        target: "antigravity-gui".to_string(),
        started_at: OffsetDateTime::now_utc(),
        ended_at: None,
        claude_argv: gui_args
            .iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect(),
        claude_path: gui_path.display().to_string(),
        claude_pid: pid,
        claude_exit_code: None,
        claude_session_id: None,
        claude_version: None,
        proxy_addr: local.to_string(),
        redact: !cli.no_redact,
        host_filter: hosts,
        pine_version: env!("CARGO_PKG_VERSION").to_string(),
        session_dir: Some(session_dir.display().to_string()),
        ca_cert_path: Some(ca_path.display().to_string()),
        cwd: std::env::current_dir().ok().map(|p| p.display().to_string()),
        user: std::env::var("USER").ok().or_else(|| std::env::var("USERNAME").ok()),
        hostname: hostname(),
        os: Some(std::env::consts::OS.to_string()),
        arch: Some(std::env::consts::ARCH.to_string()),
        shell: std::env::var("SHELL").ok(),
        term: std::env::var("TERM").ok(),
        system: Some(collect_system_info()),
    };
    logger.write_meta(&meta).await?;

    let code = supervise_child(&mut child, pid).await;
    if let Err(e) = logger
        .update_meta(|m| {
            m.ended_at = Some(OffsetDateTime::now_utc());
            m.claude_exit_code = code;
        })
        .await
    {
        warn!(error = %e, "updating meta.json on shutdown");
    }
    let elapsed = spawn_instant.elapsed();
    eprintln!(
        "\npine: antigravity (gui) exited (code={}, after {:.1}s) — session {}\n           logs at {}",
        code.map(|c| c.to_string()).unwrap_or_else(|| "signal".to_string()),
        elapsed.as_secs_f32(),
        session_id,
        session_dir.display()
    );

    // Same single-instance-lock heuristic as Windsurf: an ~instant exit(0)
    // means the launch was handed off to an existing instance and our proxy/CA
    // env was dropped.
    if elapsed < std::time::Duration::from_secs(3) && code == Some(0) {
        eprintln!(
            "\npine: ⚠  antigravity exited almost immediately. Most likely another\n\
             \x20            Antigravity instance was alive and the new launch was forwarded\n\
             \x20            to it via Electron's single-instance lock — dropping our proxy/CA.\n\
             \x20  Try:\n\
             \x20    1. Fully quit Antigravity, then re-run:\n\
             \x20         pkill -9 -i antigravity && sleep 1 && pine agy --gui\n\
             \x20    2. Use an isolated profile (bypasses the singleton lock):\n\
             \x20         pine agy --gui --user-data-dir /tmp/agy-tap\n"
        );
    }

    proxy_task.abort();
    let _ = proxy_task.await;

    std::process::exit(code.unwrap_or(0));
}

async fn run_codex(cli: Cli) -> Result<()> {
    let (codex_bin, codex_args) = match cli.command {
        Some(Cmd::Codex {
            ref codex_bin,
            ref codex_args,
        }) => (codex_bin.clone(), codex_args.clone()),
        _ => unreachable!(),
    };
    paths::ensure_dir(&paths::root()?)?;
    paths::ensure_dir(&paths::sessions_dir()?)?;
    track_install_event();

    let ca = Arc::new(ca::Ca::load_or_generate()?);

    let session_id = Ulid::new().to_string();
    let logger = log::SessionLogger::create(session_id.clone(), !cli.no_redact).await?;

    let hosts = if cli.hosts.is_empty() {
        CODEX_HOSTS.iter().map(|s| s.to_string()).collect()
    } else {
        cli.hosts.clone()
    };
    let host_filter = proxy::HostFilter::new(hosts.clone());

    let listener = proxy::bind_listener(cli.port).await?;
    let local = listener.local_addr()?;
    let proxy_url = format!("http://{}", local);

    let state =
        proxy::build_state(ca.clone(), logger.clone(), host_filter, cli.passthrough_only_logged)
            .await?;

    let codex_path = launcher::resolve_codex(codex_bin.as_deref())?;
    let spec = launcher::LaunchSpec {
        claude_path: codex_path.clone(),
        args: codex_args.clone(),
        proxy_url: proxy_url.clone(),
        session_id: session_id.clone(),
    };

    let proxy_state = state.clone();
    let proxy_task = tokio::spawn(async move {
        if let Err(e) = proxy::run(listener, proxy_state).await {
            warn!(error = %e, "proxy loop exited");
        }
    });

    let session_dir = paths::session_dir(&session_id)?;
    let ca_path = paths::ca_cert_path()?;
    let banner_text = banner::Banner::new("pine · codex")
        .row("session", session_id.clone())
        .row("proxy", proxy_url.clone())
        .row("logs", banner::abbrev_path(&session_dir))
        .row("hosts", hosts.join(", "))
        .row("codex", banner::abbrev_path(&codex_path))
        .render();
    print!("{}", banner_text);
    use std::io::Write;
    let _ = std::io::stdout().flush();

    write_last_session_pointer(&session_id, &session_dir, &proxy_url)?;

    let mut child = launcher::spawn(spec).await?;
    let pid = child.id();

    let meta = log::SessionMeta {
        session_id: session_id.clone(),
        target: "codex".to_string(),
        started_at: OffsetDateTime::now_utc(),
        ended_at: None,
        claude_argv: codex_args
            .iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect(),
        claude_path: codex_path.display().to_string(),
        claude_pid: pid,
        claude_exit_code: None,
        claude_session_id: None,
        claude_version: None,
        proxy_addr: local.to_string(),
        redact: !cli.no_redact,
        host_filter: hosts,
        pine_version: env!("CARGO_PKG_VERSION").to_string(),
        session_dir: Some(session_dir.display().to_string()),
        ca_cert_path: Some(ca_path.display().to_string()),
        cwd: std::env::current_dir().ok().map(|p| p.display().to_string()),
        user: std::env::var("USER").ok().or_else(|| std::env::var("USERNAME").ok()),
        hostname: hostname(),
        os: Some(std::env::consts::OS.to_string()),
        arch: Some(std::env::consts::ARCH.to_string()),
        shell: std::env::var("SHELL").ok(),
        term: std::env::var("TERM").ok(),
        system: Some(collect_system_info()),
    };
    logger.write_meta(&meta).await?;

    let code = supervise_child(&mut child, pid).await;
    if let Err(e) = logger
        .update_meta(|m| {
            m.ended_at = Some(OffsetDateTime::now_utc());
            m.claude_exit_code = code;
        })
        .await
    {
        warn!(error = %e, "updating meta.json on shutdown");
    }
    eprintln!(
        "\npine: codex exited (code={}) — session {}\n           logs at {}",
        code.map(|c| c.to_string()).unwrap_or_else(|| "signal".to_string()),
        session_id,
        session_dir.display()
    );

    proxy_task.abort();
    let _ = proxy_task.await;

    std::process::exit(code.unwrap_or(0));
}

/// Gather static, non-privileged system facts: CPU counts, RAM, model name,
/// kernel/OS version. Anything we can't read becomes `None`.
fn collect_system_info() -> log::SystemInfo {
    let cpu_logical = std::thread::available_parallelism()
        .ok()
        .map(|n| n.get() as u64);

    let mut info = log::SystemInfo {
        cpu_logical,
        cpu_physical: None,
        cpu_brand: None,
        machine_model: None,
        total_memory_bytes: None,
        page_size_bytes: None,
        kernel_version: None,
        os_release: None,
    };

    #[cfg(target_os = "macos")]
    {
        info.total_memory_bytes = sysctl_str("hw.memsize").and_then(|s| s.parse().ok());
        info.cpu_physical = sysctl_str("hw.physicalcpu").and_then(|s| s.parse().ok());
        if info.cpu_logical.is_none() {
            info.cpu_logical = sysctl_str("hw.logicalcpu").and_then(|s| s.parse().ok());
        }
        info.cpu_brand = sysctl_str("machdep.cpu.brand_string");
        info.machine_model = sysctl_str("hw.model");
        info.page_size_bytes = sysctl_str("hw.pagesize").and_then(|s| s.parse().ok());
        info.kernel_version = sysctl_str("kern.osrelease");
        info.os_release = sysctl_str("kern.osproductversion")
            .or_else(|| sysctl_str("kern.osversion"));
    }

    #[cfg(target_os = "linux")]
    {
        if let Ok(s) = std::fs::read_to_string("/proc/meminfo") {
            for line in s.lines() {
                if let Some(rest) = line.strip_prefix("MemTotal:") {
                    let kb: Option<u64> = rest
                        .trim()
                        .split_whitespace()
                        .next()
                        .and_then(|n| n.parse().ok());
                    info.total_memory_bytes = kb.map(|k| k * 1024);
                    break;
                }
            }
        }
        if let Ok(s) = std::fs::read_to_string("/proc/cpuinfo") {
            let mut physical_ids = std::collections::HashSet::new();
            for line in s.lines() {
                if let Some(rest) = line.strip_prefix("model name") {
                    if info.cpu_brand.is_none() {
                        if let Some((_, v)) = rest.split_once(':') {
                            info.cpu_brand = Some(v.trim().to_string());
                        }
                    }
                } else if let Some(rest) = line.strip_prefix("physical id") {
                    if let Some((_, v)) = rest.split_once(':') {
                        physical_ids.insert(v.trim().to_string());
                    }
                }
            }
            if !physical_ids.is_empty() {
                info.cpu_physical = Some(physical_ids.len() as u64);
            }
        }
        info.kernel_version = uname_r();
        if let Ok(s) = std::fs::read_to_string("/etc/os-release") {
            for line in s.lines() {
                if let Some(rest) = line.strip_prefix("PRETTY_NAME=") {
                    info.os_release = Some(rest.trim_matches('"').to_string());
                    break;
                }
            }
        }
        info.page_size_bytes = page_size_bytes();
    }

    info
}

#[cfg(target_os = "macos")]
fn sysctl_str(key: &str) -> Option<String> {
    let out = std::process::Command::new("/usr/sbin/sysctl")
        .args(["-n", key])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

#[cfg(target_os = "linux")]
fn uname_r() -> Option<String> {
    let out = std::process::Command::new("uname").arg("-r").output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn track_install_event() {
    let flag_path = paths::root().unwrap_or_else(|_| dirs::home_dir().unwrap().join(".pine")).join(".installed");
    if flag_path.exists() {
        return;
    }
    
    tokio::spawn(async move {
        let api_key = env!("POSTHOG_API_KEY");
        let event_name = format!("{}_install", env!("CARGO_PKG_NAME"));
        
        let client = reqwest::Client::new();
        let _ = client.post("https://api.agentruntime.app/capture/")
            .json(&serde_json::json!({
                "api_key": api_key,
                "event": event_name,
                "distinct_id": "anonymous_user",
                "properties": {
                    "os": std::env::consts::OS,
                    "arch": std::env::consts::ARCH,
                }
            }))
            .send()
            .await;
            
        let _ = std::fs::File::create(flag_path);
    });
}



#[cfg(target_os = "linux")]
fn page_size_bytes() -> Option<u64> {
    let n = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if n > 0 { Some(n as u64) } else { None }
}

/// Best-effort hostname lookup. Tries libc on unix, falls back to env vars.
fn hostname() -> Option<String> {
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
        if rc == 0 {
            let nul = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            if let Ok(s) = std::str::from_utf8(&buf[..nul]) {
                if !s.is_empty() {
                    return Some(s.to_string());
                }
            }
        }
    }
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::env::var("COMPUTERNAME").ok())
}

/// Best-effort: run `<claude_path> --version` with a short timeout and capture
/// stdout. Returns `None` on any failure (so we never block startup on it).
fn detect_claude_version(claude_path: &std::path::Path) -> Option<String> {
    use std::process::{Command, Stdio};
    use std::time::Duration;
    let mut child = Command::new(claude_path)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    // Poll for up to ~1.5s.
    let deadline = std::time::Instant::now() + Duration::from_millis(1500);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => return None,
        }
    }
    let output = child.wait_with_output().ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

/// Drop a small text file at `~/.pine/last-session` so users can find
/// the most recent session without scrolling. Append-only? No — this is a
/// pointer, intentionally overwritten each launch. The session data itself
/// (traffic.jsonl, bodies, stream) remains on disk forever.
fn write_last_session_pointer(
    session_id: &str,
    session_dir: &std::path::Path,
    proxy_url: &str,
) -> Result<()> {
    let path = paths::root()?.join("last-session");
    let content = format!(
        "{session_id}\n{}\n{proxy_url}\n",
        session_dir.display()
    );
    std::fs::write(&path, content)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(unix)]
async fn wait_for_shutdown() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut sigint = match signal(SignalKind::interrupt()) {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut sigterm = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(_) => return,
    };
    tokio::select! {
        _ = sigint.recv() => {},
        _ = sigterm.recv() => {},
    }
}

#[cfg(not(unix))]
async fn wait_for_shutdown() {
    let _ = tokio::signal::ctrl_c().await;
}

/// Wait for the claude child to exit, but with an escalating signal
/// supervisor so the user can always escape:
///
///   1st Ctrl-C: terminal already delivered SIGINT to the foreground
///               process group; we just print a hint and keep waiting.
///   2nd Ctrl-C: send SIGTERM to claude.
///   3rd Ctrl-C: SIGKILL claude and force-exit pine (in case the
///               child can't be reaped, e.g. it's stuck in uninterruptible
///               I/O against a broken FUSE/network mount).
///
/// Returns the child's exit code if we collected it cleanly, or 130
/// ("killed by SIGINT") if we had to force-quit.
#[cfg(unix)]
async fn supervise_child(child: &mut tokio::process::Child, pid: Option<u32>) -> Option<i32> {
    use tokio::signal::unix::{signal, SignalKind};

    let mut sigint = signal(SignalKind::interrupt()).ok();
    let mut sigterm = signal(SignalKind::terminate()).ok();
    let mut sigquit = signal(SignalKind::quit()).ok();
    let mut count: u32 = 0;

    loop {
        tokio::select! {
            wait_res = child.wait() => {
                return match wait_res {
                    Ok(status) => status.code(),
                    Err(e) => {
                        warn!(error = %e, "waiting on claude child");
                        None
                    }
                };
            }
            Some(_) = async {
                match sigint.as_mut() { Some(s) => s.recv().await, None => None }
            } => { count += 1; on_signal(count, pid).await; if count >= 3 { return Some(130); } }
            Some(_) = async {
                match sigterm.as_mut() { Some(s) => s.recv().await, None => None }
            } => { count += 1; on_signal(count, pid).await; if count >= 3 { return Some(143); } }
            Some(_) = async {
                match sigquit.as_mut() { Some(s) => s.recv().await, None => None }
            } => {
                // Ctrl-\: skip straight to SIGKILL+exit; this is the
                // documented "get me out NOW" path.
                eprintln!("\npine: SIGQUIT — killing claude and exiting.");
                if let Some(p) = pid {
                    unsafe { libc::kill(p as i32, libc::SIGKILL); }
                }
                return Some(131);
            }
        }
    }
}

#[cfg(unix)]
async fn on_signal(count: u32, pid: Option<u32>) {
    match count {
        1 => {
            // The shell already delivered SIGINT to the whole foreground
            // process group, so claude has it. We just inform the user
            // about the escape hatch.
            eprintln!(
                "\npine: caught Ctrl-C. Press again to SIGTERM claude, a third time to force-quit."
            );
        }
        2 => {
            eprintln!("\npine: sending SIGTERM to claude. One more Ctrl-C will SIGKILL.");
            if let Some(p) = pid {
                unsafe { libc::kill(p as i32, libc::SIGTERM); }
            }
        }
        _ => {
            eprintln!("\npine: SIGKILLing claude and force-exiting.");
            if let Some(p) = pid {
                unsafe { libc::kill(p as i32, libc::SIGKILL); }
            }
            // Give the kernel a moment to deliver, then bail. We bypass
            // the normal cleanup because by definition we got here only
            // because the normal path was stuck.
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    }
}

#[cfg(not(unix))]
async fn supervise_child(child: &mut tokio::process::Child, _pid: Option<u32>) -> Option<i32> {
    // Windows: rely on tokio's ctrl_c handler to kill the child.
    let mut count: u32 = 0;
    loop {
        tokio::select! {
            wait_res = child.wait() => {
                return match wait_res { Ok(s) => s.code(), Err(_) => None };
            }
            _ = tokio::signal::ctrl_c() => {
                count += 1;
                if count == 1 {
                    eprintln!("\npine: caught Ctrl-C. Press again to kill claude.");
                } else {
                    eprintln!("\npine: killing claude.");
                    let _ = child.start_kill();
                    let _ = tokio::time::timeout(
                        std::time::Duration::from_secs(1), child.wait()
                    ).await;
                    return Some(130);
                }
            }
        }
    }
}
