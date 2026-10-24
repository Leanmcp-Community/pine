<p align="center">
  <img src="assets/pine-logo.svg" width="52" alt="Pine logo">
</p>

<h1 align="center">Pine</h1>

<p align="center"><b>Multiplayer AI.</b> Your Claude Code sessions, live and shared with everyone who can read the same GitHub repo.</p>

<p align="center"><a href="https://usepine.dev">usepine.dev</a> · <a href="https://app.usepine.dev">app.usepine.dev</a></p>

---

Pine runs your AI coding agent (Claude Code by default, also Codex, Antigravity and Windsurf) through a local HTTPS proxy.
- **Recording:** every request and response is recorded locally to `~/.pine/sessions/<id>/`, always.
- **Sharing:** when you're logged in, each session's **conversation** streams to Pine: a compact transcript of your messages, the replies and one-line tool calls, built on your machine. The raw capture never leaves it. Teammates can then watch them live, name checkpoints, and pull them to their own machine.
- **Access:** who can see a session follows **GitHub repo access**. Can you read the repo? Then you see its sessions.

`pn` is a short alias for `pine`.

## Install (macOS and Linux)

```bash
curl -fsSL https://usepine.dev/install | bash
```

- **Where it goes:** `pine` and `pn` are installed to `~/.local/bin`, without sudo.
- **What it downloads:** the release binary for your OS/arch (Linux x86_64/ARM64 with glibc 2.35+, or macOS Intel/Apple Silicon). It verifies the binary's SHA-256 checksum.
- **No binary for your platform:** it builds from source with Cargo instead.
- **`pn` already taken:** if another `pn` command already exists on your machine, only `pine` is installed.
- **Background sync:** it also turns on `pn watch` (a login job that runs as you, no sudo), which shares sessions from the Claude and Codex desktop apps and plain `claude` / `codex` too. Skip it with `--no-watch`; turn it off later with `pn watch --uninstall`, and updates leave it off.

Other options:

```bash
bash scripts/install.sh --version edge-58-a1b2c3d4e5f6   # pin a build
bash scripts/install.sh --source                         # build from source
cargo install --path . --locked                          # from a checkout (or ./install.sh, which also adds pn)
```

To update, run `pine update` or re-run the installer. To uninstall, delete `~/.local/bin/pine` and `~/.local/bin/pn`.

## Quick start

```bash
pn login          # opens github.com, approve the code → sync turns on
pn                # run Claude Code through Pine; the session streams to app.usepine.dev
pn checkpoint "rate-limit-working" -m "tests green"
pn pull rate-limit-working     # on a teammate's machine
```

Only sessions started after `pn login` are shared. To share older ones on purpose, run `pn sync --all`.

## Commands

| Command | What it does |
|---|---|
| `pn` | Launch `claude` through the proxy. Args after `--` go to claude. |
| `pn codex`, `pn agy`, `pn windsurf` | Same for OpenAI Codex, Antigravity (`--gui` for the desktop app) and Windsurf. |
| `pn proxy` | Run only the proxy and print the env vars to wire up any client. |
| `pn login` / `pn logout` | GitHub device-flow login to Pine / revoke this device. Recording continues locally either way. |
| `pn whoami` | Who you are, and whether sync is live, paused or local-only (works offline). |
| `pn sync` | Upload now and keep watching: sessions run through `pn`, plus Claude Code's and Codex's own sessions (plain `claude` / `codex` and both desktop apps). Since login by default; `--all` for everything. Also `--once`, `status`, `pause`, `resume`. |
| `pn watch --install` | Run that sync in the background: starts now and at every login (launchd on macOS, systemd on Linux), no terminal needed. `pn watch status` shows if it's running and its last log lines; `pn watch --uninstall` stops it. Log: `~/.pine/watch.log`. |
| `pn checkpoint "name" [-m note] [--session id]` | Name the current conversation at this moment. |
| `pn checkpoints` | Your team's checkpoints. |
| `pn pull <checkpoint-name \| session-id> [--into claude\|codex]` | Continue any session you can see, in Claude Code or Codex: downloads the conversation and adds it as a native session (`claude --resume` / `codex resume` / the Codex app) in your clone of its repo, found automatically. Offers to open it right away. |
| `pn handoff claude` / `pn handoff codex` `[n \| id] [--all]` | Continue a session in the other agent, on this machine. `claude`: pick a Codex session (CLI or desktop app) in this folder and add it to `claude --resume`. `codex`: pick a Claude Code session and add it to `codex resume` and the Codex app. Nothing is uploaded. |
| `pn open [id]` | Open a session (default: the latest) on app.usepine.dev. |
| `pn -i` | Interactive menu: login, sync, checkpoints, sessions. |
| `pn ca trust` / `pn ca untrust` | Add or remove Pine's root CA in the macOS login keychain. |
| `pn update` | Replace this binary with the latest release. The background job (`pn watch`) also checks every 10 minutes and installs new releases by itself; `pn update --auto off` turns that off, `--auto on` back on. |

## Teams and access

- **Setup:** an org admin installs the **Pine MD** GitHub App on the org (all repos, or selected ones). Pine only reads repo metadata and org membership, never code.
- **Joining:** everyone in the org signs in with GitHub. The **Team** page on app.usepine.dev shows who has joined and who has the CLI connected, plus an invite message to copy.
- **Tagging:** each session is tagged with the repo, branch and commit of the folder it ran in, read from `git remote get-url origin`.
- **Who can see a session:** you can see it if it's yours, or if you can currently read its repo on GitHub. Losing repo access removes access to its sessions.
- **Private sessions:** sessions outside a GitHub repo, or in repos where Pine MD isn't installed, stay private to their owner.

## What gets captured

Each target has a default host allow-list. Other hosts are tunneled without being decrypted. Override the list with `--hosts a.com,*.b.com`.

- **claude:** `api.anthropic.com`, `*.anthropic.com`
- **codex:** `api.openai.com`, `*.openai.com`, `chatgpt.com`, `*.chatgpt.com`
- **antigravity:** Google Cloud Code / AI Platform hosts, Google auth hosts, and `api.anthropic.com`
- **windsurf:** `*.codeium.com`, `*.windsurf.com` and related hosts

What gets redacted and how it's stored:
- **Redaction:** auth headers (`authorization`, `x-api-key`, cookies) are hashed instead of stored, unless you pass `--no-redact`.
- **Storage:** sessions are append-only and never deleted. The layout is `meta.json`, `traffic.jsonl`, `bodies/`, `stream/`.
- **Protocol:** the proxy decrypts HTTP/1.1 only. Pure HTTP/2 (gRPC) calls are tunneled without decryption.

**Trusting the CA.** Claude Code and Codex get the CA through environment variables (`NODE_EXTRA_CA_CERTS`). `agy` and Windsurf only trust the OS store, so run `pn ca trust` once for those. On Linux, `pn ca trust` prints the manual steps instead of changing the system store.

## Your data

- **Local first:** everything is recorded locally whether or not you're logged in.
- **Sharing:** logged-in sessions are visible to everyone who can read the repo. They include prompts, code and tool output. Structured credentials are filtered, but free text can still contain secrets.
- **Pausing uploads:** run `pn sync pause` to stop uploading.
- **Account files:** `~/.pine/auth.json` (mode 600) holds this device's Pine token. You can sign a device out from the **Devices** page.
- **Usage events:** pn tells us which command ran (`pull`, `handoff`, `watch`…), its version and OS, through PostHog. Never arguments, paths, prompts or code. Logged in, you're identified by your GitHub login. Turn it off with `pn telemetry off`, `PINE_TELEMETRY=0` or `DO_NOT_TRACK=1`.

## Self-hosted uploads

Before Pine's hosted app, Pine had a self-hosted upload server (`pine cloud …`, FastAPI + SQLite). It still works. See [README_CLOUD.md](README_CLOUD.md).

## Development

```bash
cargo test --locked
python3 -m unittest discover -s tests   # installer / packaging tests
```

Every push to `main` builds Linux (x86_64, ARM64) and macOS (Intel, Apple Silicon) binaries. Each build is published as a GitHub release tagged `edge-<n>-<commit>` (`.github/workflows/release-edge.yml`). That's what the installer and `pine update` download. Tagged `vX.Y.Z` releases are built by `release-linux.yml`.

The web app (app.usepine.dev) lives in a separate repo.
