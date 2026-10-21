# Plan — Windsurf support (`windsurftap` / `pine --target windsurf`)

Goal: capture every HTTPS request/response Windsurf (Cognition's VS Code fork
Electron app) makes — to `*.codeium.com`, `*.windsurf.com`, `*.cognition.dev`,
model providers it proxies through, telemetry, auth, marketplace, etc. — into
`~/.pine/sessions/<id>/` the same way we already do for Claude Code.

The proxy core (`proxy.rs`, `ca.rs`, `log.rs`, `sse.rs`) is already
target-agnostic — it MITMs whatever connects to it. The work is almost entirely
in **how we route Windsurf's traffic to our proxy** and **what hostnames we
default-tap**. There is no code change to the TLS interception path.

---

## 1. The constraint that decides the architecture

Windsurf is **Electron / Chromium** (a VS Code fork). That gives us very
different injection surface from `claude`'s hardened-runtime Bun binary:

- Electron's net stack honors `--proxy-server=` and `--proxy-bypass-list=`
  Chromium switches, plus `HTTPS_PROXY` / `HTTP_PROXY` env vars for the
  Node-side (extension host, language servers, update checks, telemetry that
  goes through Node `http`/`undici`/`fetch`).
- Chromium does **not** read `SSL_CERT_FILE` / `NODE_EXTRA_CA_CERTS` for the
  browser-side network stack. It uses the OS trust store. So our CA must be
  trusted by the OS keychain (macOS) / NSS (Linux) / SChannel (Windows), or
  installed per-user via `--ignore-certificate-errors-spki-list` (fragile,
  Chromium-only) — **the only sustainable answer is OS-trust install of our
  root CA**.
- The Node side inside Electron (extension host, integrated terminal child
  procs, language servers spawned by extensions) **does** honor
  `NODE_EXTRA_CA_CERTS`, so we still set it for child processes.
- Hardened runtime / dyld injection is irrelevant here — we don't need it.
  Pure proxy-based MITM is enough, no library injection.

So: **same proxy. Different launcher. Plus a one-time `ca trust` step.**

---

## 2. CLI shape

Two viable shapes; recommend (a):

**(a) Single binary, `--target` flag (preferred)**

```
pine [--target claude|windsurf] [common flags] [-- <child args>]
pine windsurf [common flags] [-- <windsurf args>]   # subcommand alias
```

- Default `--target claude` keeps current behavior bit-for-bit.
- `--target windsurf` swaps:
  - default host list → Windsurf hosts (§4)
  - launcher → `launcher::windsurf::spawn` (§3)
  - banner title → "pine · windsurf"
- New flags scoped to windsurf:
  - `--windsurf-bin <PATH>` (defaults to `which windsurf`, then the standard
    install paths in §3.1)
  - `--user-data-dir <PATH>` (lets us run an isolated profile so we don't
    break the user's real Windsurf install during testing)

**(b) Separate `windsurftap` binary**

Cleaner UX, but doubles release surface and divergent CLI. Skip unless we
later split into a plugin model.

Rename the crate description from "Claude Code's HTTPS traffic" to "Claude
Code & Windsurf HTTPS traffic". Keep the binary name `pine`; add
`windsurftap` as a thin alias (cargo `[[bin]]` that calls `main` with
`--target windsurf` injected) for ergonomics.

---

## 3. Launching Windsurf under the proxy

New module `src/launcher/windsurf.rs` (and refactor existing `launcher.rs` →
`launcher/claude.rs` + `launcher/mod.rs` with a shared `LaunchSpec`).

### 3.1 Resolving the binary

Search order:
1. `--windsurf-bin` if given.
2. `which windsurf` (CLI shim Cognition installs).
3. Platform defaults:
   - macOS: `/Applications/Windsurf.app/Contents/MacOS/Electron`
     (the actual executable inside the .app — NOT `open -a Windsurf`, because
     `open` detaches and we lose the child PID + env propagation).
   - Linux: `/usr/share/windsurf/windsurf`, `/opt/windsurf/windsurf`,
     `~/.local/share/windsurf/windsurf`.
   - Windows: `%LOCALAPPDATA%\Programs\Windsurf\Windsurf.exe`.

We must launch the **actual binary**, not the `.app` bundle, so:
- env vars propagate (macOS `open` strips most),
- we can wait on the child PID and run our supervisor,
- we get one Electron instance attached to us (not "focus the existing one").

If a Windsurf instance is already running, a second launch will silently
attach to the existing process and our env vars are dropped on the floor.
**Detect this and refuse** with a clear message: "Windsurf is already running.
Quit it first (⌘Q) or pass `--user-data-dir <fresh>` to run an isolated
instance." Detection: scan `pgrep -f Windsurf` / equivalent.

### 3.2 Env + Chromium switches

Set both — Electron's main process honors env, but the Chromium net stack is
configured via switches and we want belt-and-braces:

```
HTTPS_PROXY=http://127.0.0.1:<port>
HTTP_PROXY=http://127.0.0.1:<port>
ALL_PROXY=http://127.0.0.1:<port>
NO_PROXY=localhost,127.0.0.1,::1
NODE_EXTRA_CA_CERTS=<ca pem>          # for Node-side / extension host
SSL_CERT_FILE=<ca pem>                # belt-and-braces for child procs
GRPC_DEFAULT_SSL_ROOTS_FILE_PATH=...  # for any gRPC clients (Codeium has used gRPC)
PINE_SESSION=<ulid>
```

CLI args appended before `--`:

```
--proxy-server=http://127.0.0.1:<port>
--proxy-bypass-list=<-loopback>;localhost;127.0.0.1;::1
--user-data-dir=<isolated dir, only if --user-data-dir set>
```

Forward user `claude_args` (rename `child_args`) after these.

### 3.3 Process supervision

Reuse the escalating-Ctrl-C supervisor from `main.rs`. Electron has its own
SIGINT handling but the existing 1→SIGTERM→SIGKILL ladder still works. The
only twist: Electron forks a tree (zygote + GPU + utility + renderer
processes). On force-kill we should `kill -KILL -<pgid>` rather than just the
parent PID, otherwise orphaned helpers linger. Spawn the child in its own
process group (`setpgid`), then signal the group.

---

## 4. Default tap host list

Bake an opinionated default. Users can override with `--hosts`.

```rust
const WINDSURF_HOSTS: &[&str] = &[
    // Cognition / Codeium first-party
    "*.codeium.com",
    "*.codeiumdata.com",
    "*.windsurf.com",
    "*.cognition.dev",
    "*.codeium.dev",
    "server.codeium.com",
    "inference.codeium.com",
    "exa.codeium.com",
    "telemetry.codeium.com",

    // Auth / accounts
    "auth.codeium.com",
    "login.windsurf.com",

    // VS Code marketplace (Windsurf uses Open VSX by default, but also
    // Microsoft marketplace if reconfigured)
    "open-vsx.org",
    "*.open-vsx.org",
    "marketplace.visualstudio.com",
    "*.vsassets.io",
    "*.gallerycdn.vsassets.io",

    // Updates / telemetry the VS Code fork inherits
    "update.windsurf.com",
    "*.windsurfusercontent.com",

    // Common upstream model providers Windsurf may forward to
    "api.openai.com",
    "api.anthropic.com",
    "*.anthropic.com",
    "generativelanguage.googleapis.com",
];
```

**Action item before shipping**: validate this list by running Windsurf once
with `--passthrough-only-logged=false` (default, blind-tunnel everything) and
inspecting `~/.pine/sessions/<id>/traffic.jsonl` for which hosts the
CONNECT log shows. Add anything we missed; remove anything that's never seen.
Treat the list above as a starting hypothesis, not gospel.

---

## 5. CA trust — the real UX problem

Today `pine ca path` prints the path; the user is expected to do nothing
because Bun honors `SSL_CERT_FILE`. For Chromium that doesn't work.

Add `pine ca trust` / `pine ca untrust`:

- **macOS**: `security add-trusted-cert -d -r trustRoot -k ~/Library/Keychains/login.keychain-db <ca.pem>`
  (no sudo, user keychain). Removal: `security delete-certificate -c "pine Root CA"`.
- **Linux**: copy CA into `~/.pki/nssdb` via `certutil` (NSS — Chromium's
  store), AND `/usr/local/share/ca-certificates/` + `update-ca-certificates`
  for system-wide. NSS path doesn't need root; system path does. Print which
  the user wants and prompt.
- **Windows**: `certutil -user -addstore Root <ca.crt>` (no admin; per-user
  store, Chromium reads it).

Print clearly which keychain we're touching, and how to undo.

`pine --target windsurf` should, on first run, **detect that the CA is
not OS-trusted** and either auto-run `ca trust` (with confirmation) or print
the exact command and exit. Don't silently launch Windsurf and have every
request fail with `NET::ERR_CERT_AUTHORITY_INVALID`.

Detection approach: spawn `curl --cacert <ca> https://<known-tapped-host>/`
through the proxy from the user's shell once and check; or (simpler) do a
TLS handshake in Rust against `https://localhost:<port>` using the OS trust
store via `rustls-native-certs` and see if our forged cert verifies.

---

## 6. Extension host & language servers

Windsurf, like VS Code, spawns child processes:
- the extension host (Node)
- language servers (rust-analyzer, gopls, pyright, etc.)
- task-runner shells (integrated terminal)

The Electron parent's env propagates to children, so `HTTPS_PROXY` +
`NODE_EXTRA_CA_CERTS` already cover Node-based extensions and the integrated
terminal. Two known gotchas:

1. **Some extensions hard-code `https.Agent({ rejectUnauthorized: true })`**
   without honoring `NODE_EXTRA_CA_CERTS`. We can't fix those generically;
   document as a known limitation.
2. **Language servers in compiled languages** (rust-analyzer in Rust, gopls
   in Go) won't read `NODE_EXTRA_CA_CERTS`. They use the OS trust store on
   macOS/Windows (so once `ca trust` ran, they're fine), and `SSL_CERT_FILE`
   on Linux for Go's net/http (set already).

Out of scope for v1: capturing renderer-process WebSocket traffic to
`*.codeium.com`. Chromium routes WS through `--proxy-server` as long as it's
not a `wss://` over an explicit DIRECT rule. Verify during §4 validation.

---

## 7. Logging / session metadata changes

`SessionMeta` currently has `claude_path`, `claude_pid`, `claude_argv`,
`claude_exit_code`, `claude_session_id`, `claude_version`. Generalize:

- Add `target: "claude" | "windsurf"`.
- Rename fields to `child_*` and keep serde aliases for the old names so
  existing logs still parse. Or keep `claude_*` fields populated for the
  claude target and add parallel `windsurf_*` fields. Aliases are cleaner.
- `child_version` for Windsurf: read `package.json` next to the binary or
  parse `windsurf --version` (works on the CLI shim, not always on the
  Electron binary).

Don't rename anything visible in `traffic.jsonl` — that's just HTTP records,
target-agnostic.

---

## 8. Banner / `proxy` subcommand

The `proxy` subcommand already prints copy-pasteable env vars. Add a hint
block when `--target windsurf` (or always — it's cheap):

```
# To capture an already-running Windsurf, quit it (⌘Q) and relaunch with:
/Applications/Windsurf.app/Contents/MacOS/Electron \
  --proxy-server=http://127.0.0.1:<port> \
  --proxy-bypass-list='<-loopback>;localhost;127.0.0.1'
# (CA must be trusted in the OS keychain — run: pine ca trust)
```

---

## 9. Work breakdown

1. Refactor `launcher.rs` into `launcher/{mod,claude,windsurf}.rs`. Pull the
   shared env-setting code into `launcher::common`. **No behavior change.**
2. Add `--target` flag + `windsurf` subcommand in `main.rs`. Default stays
   `claude`. Branch on target for: default hosts, launcher choice, banner
   title, meta `target` field.
3. Implement `launcher::windsurf::resolve_windsurf` (path search) and
   `spawn` (env + Chromium switches + process group). Implement
   "is Windsurf already running" detection.
4. Implement `pine ca trust` / `ca untrust` per-platform. macOS first
   (matches our test box), then Linux, then Windows.
5. First-run trust check on `--target windsurf`: bail with a clear error if
   the CA isn't OS-trusted, suggesting `pine ca trust`.
6. Default Windsurf host list (§4). Run a discovery session with the list
   wide-open, harvest CONNECT log, prune. Commit the pruned list.
7. `SessionMeta` generalization with serde aliases (§7). Bump
   `pine_version`; old logs must still deserialize — add a unit test.
8. README / `--help` updates. Drop a short "Windsurf" section pointing at
   `pine ca trust` as a prerequisite.
9. Manual smoke: launch Windsurf via `pine --target windsurf`, sign in,
   ask Cascade something, accept a completion, install an extension.
   Confirm `traffic.jsonl` has request/response bodies for each step and no
   `tunnel-only` rows for hosts we expected to MITM.

---

## 10. Known risks / things to watch

- **Auto-update**: Windsurf may auto-update mid-session; the updater forks a
  helper that re-execs the new binary. Our supervisor will lose the child.
  Document; don't try to chase the new PID in v1.
- **WebSocket capture**: our proxy logs HTTP request/response bodies. If
  Windsurf streams Cascade results over WS, we'll see the upgrade but not
  decoded frames unless we extend `proxy.rs`. Note in v1; tackle in v2 only
  if WS turns out to carry the interesting payload.
- **Cert pinning**: if Cognition pins certs in their gRPC client, MITM
  fails for those endpoints regardless of OS trust. We'll see TLS handshake
  errors in the proxy log; the answer is "blind-tunnel that host." Add to
  default `--passthrough-only-logged=false` recommendation for Windsurf.
- **Login flows that open a browser**: the OAuth callback hits the user's
  default browser, not Windsurf, so we won't capture the redirect. The
  token-exchange call back into Windsurf *will* be captured. Acceptable.
- **Non-isolated profile**: if the user runs `pine --target windsurf`
  on their daily-driver Windsurf, our proxy + CA-trust changes affect their
  real install. Document `--user-data-dir` for safer experimentation.

---

## 11. Out of scope (v1)

- Cursor support (same Electron/VS Code fork shape; should fall out of this
  work with a different host list — punt to v2).
- Auto-CA-untrust on uninstall.
- WS frame decoding.
- Per-extension allow/deny of MITM.
