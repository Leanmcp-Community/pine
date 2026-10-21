# Why `pine agy` only opens the CLI (not the Antigravity GUI)

## TL;DR

`pine agy` is **hard-wired to launch the `agy` CLI binary only** — the
self-contained Go agent CLI at `~/.local/bin/agy`. It does **not** launch the
Antigravity desktop app at `/Applications/Antigravity.app`. The GUI is a
separate Electron/Chromium program that pine never starts, so you only ever
see the terminal CLI. This is by design, not a bug.

## What's actually on disk

You have **two** different things called "Antigravity":

| Thing | Path | What it is |
|-------|------|------------|
| `agy` CLI | `~/.local/bin/agy` | Google's Antigravity **agent CLI** — a Mach-O / Go binary. Terminal-only. |
| Antigravity GUI | `/Applications/Antigravity.app` | The Electron/Chromium **desktop IDE**. |

`pine agy` resolves and spawns the **first one only**.

## Where this is decided in the code

`pine agy` → `Cmd::Antigravity` → `run_antigravity()` → `resolve_antigravity()`
+ `spawn_antigravity()`.

- `src/launcher.rs::resolve_antigravity()` explicitly resolves the `agy` Go
  binary. Its own doc comment says:

  > `agy` is a self-contained Go binary (Google's Antigravity agent CLI), **not
  > the Electron GUI under `/Applications/Antigravity.app`**.

  It searches `$PATH`, then `~/.local/bin/agy`, `~/bin/agy`,
  `/usr/local/bin/agy`, `/opt/homebrew/bin/agy`. Notice the `.app` bundle is
  never in that list.

- `src/launcher.rs::spawn_antigravity()` launches that binary with the proxy
  wired in **via environment variables** (`HTTPS_PROXY`, `HTTP_PROXY`,
  `ALL_PROXY`, plus `NODE_EXTRA_CA_CERTS` / `SSL_CERT_FILE` for TLS trust).

There is simply no code path in the `agy` subcommand that opens the desktop app.

## Why it's done this way (env-var proxy vs. the GUI)

The whole point of the `agy` path is to **tap the CLI's LLM traffic** with the
least friction:

1. **`agy` is a Go binary.** Go's `net/http` automatically honors
   `HTTP(S)_PROXY` / `NO_PROXY` from the environment. So pine just sets
   those env vars and the CLI's traffic flows through the proxy — no launch
   flags needed.

2. **The Electron GUI ignores those env vars.** A Chromium app does *not* route
   its network through `HTTP_PROXY`; it needs a `--proxy-server=...` command
   line flag and its own CA handling. (That's exactly the more involved dance
   the `pine windsurf` path does: kill the running instance, relaunch with
   `--proxy-server`, manage a separate user-data dir, etc.)

3. **On macOS, Go validates TLS against the system keychain.** That's why the
   `agy` path insists the pine root CA be OS-trusted (`pine ca trust`)
   and prints the `root CA is trusted by the OS keychain ✓` line you saw.

So: the CLI is the easy, reliable thing to MITM, and that's what the `agy`
command was built to do.

## "But I expected a window to pop up"

Two things worth knowing:

- Running `agy` **outside** pine may *also* drive the desktop app — the
  spawn code notes that `agy` "can launch helper processes (and drive the
  Electron app over CDP)" [Chrome DevTools Protocol]. So in normal use the CLI
  and GUI can talk to each other. Under pine you're getting the CLI side
  only.
- If the GUI never opens at all even outside pine, that's an Antigravity
  install/launch issue separate from pine — try opening
  `/Applications/Antigravity.app` directly from Finder to confirm it runs.

## The fix (now implemented): `pine agy --gui`

`pine agy` now takes a `--gui` flag that launches the **Antigravity
desktop app** (the Electron IDE) under the proxy instead of the `agy` CLI:

```sh
pine ca trust          # one-time: Chromium needs the root CA OS-trusted
pine agy --gui         # launches /Applications/Antigravity.app under the proxy
```

Under the hood this reuses the exact Chromium-launch machinery the `windsurf`
command uses (`src/launcher.rs::spawn_electron`):

1. Resolves the Electron binary at
   `/Applications/Antigravity.app/Contents/MacOS/Antigravity`
   (`resolve_antigravity_gui`; override with `--gui-bin`).
2. Launches it with `--proxy-server=http://127.0.0.1:<port>` (Chromium ignores
   `HTTP_PROXY` env vars — that's why the old CLI-only env-var approach never
   touched the GUI).
3. Kills any already-running Antigravity instance first, because Electron's
   single-instance lock would otherwise forward the launch to the existing
   process and silently drop our proxy/CA settings.

Extra flags (all GUI-only): `--user-data-dir <dir>` (isolated profile, bypasses
the singleton lock), `--skip-trust-check`, `--no-restart`.

## Proof that `agy` is a terminal app, not a GUI (from `strings agy`)

Running `strings` over the 135 MB `agy` binary confirms the design above:

- **It's a Go binary.** Go runtime markers are all over it: `GOGC`, `goroutine`
  / `bad g`, `gcBits`, `runtime`, `cas1..cas6`, `pc=`, `sp=`, `morestack`.
- **It's a full-screen terminal (TUI) app, not a window.** It carries
  terminfo/termcap capability strings — keypad keys `kp0`..`kp9`, function keys
  `f10`..`f63`, cursor-movement caps (`cuf`/`cuu`/`smcup`), ANSI/`ESC`
  sequences. A GUI app would instead show V8/Blink/Chromium strings — there are
  none.
- **It speaks the network directly** (`POST`, `HEAD`, `h2c`/HTTP-2, `gRPC`,
  JSON, websocket `ws2`/`ws3`) and references **MCP** — consistent with a Go
  agent CLI that calls Google's Cloud Code / Gemini backend.
- The giant tables of file extensions and language names (`.rs`, `.go`, `.py`,
  …) are its **syntax highlighter** for rendering code in the terminal.

So `agy` *is* the CLI by nature — it was never going to open a window. The
desktop GUI is the separate Electron app at `/Applications/Antigravity.app`,
which is exactly what `pine agy --gui` now launches.

## One-line answer

> `pine agy` spawns the `agy` **Go CLI binary** (proxy wired in via
> `HTTP_PROXY` env vars), not `/Applications/Antigravity.app`. The Electron GUI
> is a separate program pine doesn't launch — and it wouldn't honor the
> env-var proxy anyway; it'd need Chromium `--proxy-server` flags like the
> `windsurf` path uses.
