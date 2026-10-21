# pine for Windsurf — beginner's guide

This guide assumes **nothing**. By the end, every HTTPS request Windsurf makes
(to Cognition's servers, to model providers, to the marketplace, to telemetry
endpoints, etc.) will be saved to disk under `~/.pine/sessions/<id>/` so
you can inspect it later.

**Time required:** ~5 minutes the first time, ~5 seconds every time after.

---

## What you need

1. **macOS** (these instructions are macOS-specific; Linux/Windows users — see §"Other platforms" at the end).
2. **Rust toolchain.** If `cargo --version` works in your terminal, you're set. Otherwise install via:
   ```bash
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
   ```
   then close and reopen your terminal.
3. **Windsurf installed** — download from <https://windsurf.com/download>. Drag `Windsurf.app` into `/Applications/` like any other Mac app.

That's it. You don't need to be an admin, and nothing here uses `sudo`.

---

## Step 1 — Install `pine`

From the directory containing this README:

```bash
./install.sh
```

That builds and copies the binary to `~/.cargo/bin/pine`. If that
directory isn't on your `$PATH`, the installer will print the one line you
need to add to your `~/.zshrc`. Add it, then reopen your terminal.

Verify:

```bash
pine --version
pine --help
```

---

## Step 2 — Trust the pine root certificate (one time, ever)

### What this is and why you need it

`pine` works by sitting between Windsurf and the internet. To read
HTTPS traffic, it has to decrypt it — which means presenting Windsurf with
a fake TLS certificate for every site it visits. Those fake certificates
are signed by a **root CA** that lives only on your machine, in
`~/.pine/ca/root.crt`.

For Windsurf (which is built on Chromium, the same engine as Chrome) to
accept these fake certs, your Mac's keychain has to trust that root CA.

This is a one-time setup. The CA is generated locally, never leaves your
machine, and only signs certs for the duration of `pine` sessions you
explicitly start.

### Do it

```bash
pine ca trust
```

macOS will pop up a dialog asking for your **login password** (the same one
you use to unlock your Mac). Enter it. Confirm. That's it.

You can verify:

```bash
pine ca path        # where the CA cert lives
pine ca print       # print the cert PEM to your terminal
```

### If you ever want to undo

Two levels of undo:

```bash
pine ca untrust     # remove from keychain, but keep the CA file on disk
pine ca reset       # remove from keychain AND delete ~/.pine/ca/
```

After `reset`, the next time you run pine a brand-new root CA will be
minted, and you'll need to `pine ca trust` again. Use `reset` if
something is wrong with your CA and you want a clean slate.

---

## Step 3 — Quit Windsurf if it's already running

This matters. Electron apps like Windsurf use **single-instance mode** — if
Windsurf is already running and you launch another copy, the new one just
tells the existing one to come to the foreground and then quits. Our proxy
settings get dropped on the floor.

So before every `pine windsurf` run: **quit Windsurf completely** (⌘Q
in the menu bar, or right-click the Dock icon → Quit).

`pine windsurf` will refuse to launch if it detects Windsurf is
already running and tell you to quit it. You can override that with
`--user-data-dir <some/empty/dir>` to launch an isolated instance with its
own profile, but for first-time users: just quit the regular one.

---

## Step 4 — Launch Windsurf under pine

```bash
pine windsurf
```

You'll see a banner like:

```
┌── pine · windsurf ──────────────────────────────────┐
│ session    01HXY...                                      │
│ proxy      http://127.0.0.1:54321                        │
│ logs       ~/.pine/sessions/01HXY.../               │
│ hosts      *.codeium.com, *.windsurf.com, ...            │
│ windsurf   /Applications/Windsurf.app/...                │
└──────────────────────────────────────────────────────────┘
pine: root CA is trusted by the OS keychain ✓
```

Windsurf will open. Use it normally — sign in, ask Cascade things, accept
completions. Every HTTPS call it makes that matches the host list is being
captured to disk.

When you quit Windsurf (⌘Q), `pine` exits too and prints the path to
your session logs.

---

## Step 5 — Find your logs

```bash
cat ~/.pine/last-session
```

That prints the most recent session ID and its directory. Inside that
directory:

```
meta.json          # session metadata (PID, start time, proxy port, ...)
traffic.jsonl      # one JSON line per request/response, with bodies
bodies/            # large request/response bodies that didn't fit inline
stream/            # SSE event logs for streaming responses
```

Quick exploration:

```bash
SESSION_DIR=$(awk 'NR==2' ~/.pine/last-session)

# All unique hosts Windsurf called
jq -r '.url' "$SESSION_DIR/traffic.jsonl" \
  | sed -E 's|https?://([^/]+).*|\1|' | sort -u

# All POSTs (likely the interesting stuff)
jq 'select(.method == "POST") | {url, status: .response.status}' \
   "$SESSION_DIR/traffic.jsonl"

# Pretty-print the first request
jq 'head' "$SESSION_DIR/traffic.jsonl"   # use head -1 if jq is older
```

`traffic.jsonl` is plain newline-delimited JSON. Bodies are inlined under
`request.body_inline.data` / `response.body_inline.data` if small,
otherwise spilled to `bodies/<id>.{req,res}.bin` and the path is in
`request.body_path` / `response.body_path`.

**Logs are kept forever.** Nothing is rotated, truncated, or auto-deleted.
If a session gets noisy, just `rm -rf ~/.pine/sessions/<id>/`.

---

## Common workflows

### Discovery run — capture *everything* once to find unknown endpoints

```bash
pine windsurf --hosts '*'
```

`*` means "MITM every host." Quote it so your shell doesn't expand it.
This is exactly the same as a normal run, just with a wide-open host
filter. After your discovery session, run normally (no `--hosts`) to go
back to the curated list.

### Same, but isolated profile (won't touch your real Windsurf settings)

```bash
mkdir -p /tmp/wsf-test
pine windsurf --user-data-dir /tmp/wsf-test
```

Useful if you don't want to quit your real Windsurf, or if you want to
test things without touching your real account.

### Just the proxy, no Windsurf

```bash
pine proxy
```

Stays up until Ctrl-C. Prints the env vars (`HTTPS_PROXY`,
`NODE_EXTRA_CA_CERTS`, …) you can `export` in another shell to route any
other client through it.

---

## Troubleshooting

### "Windsurf is already running. Quit it first…"

You have a Windsurf instance running. ⌘Q it (or right-click Dock → Quit)
and try again. Or pass `--user-data-dir <dir>` to run a separate instance.

### "WARNING — root CA is NOT trusted by the OS keychain"

You skipped Step 2, or it didn't work. Run `pine ca trust`. If the
keychain dialog never appeared, run with verbose tracing:

```bash
PINE_LOG=pine=debug pine ca trust
```

### Windsurf opens but everything fails with "certificate authority invalid"

Almost always = CA isn't trusted. Run `pine ca trust` and relaunch.

### Nothing is being logged in `traffic.jsonl`

Check the banner's `proxy` line is `http://127.0.0.1:<port>` and that
Windsurf is actually using it (you'll see CONNECTs in the log even for
non-MITM hosts). If `traffic.jsonl` is completely empty, Windsurf isn't
honoring the proxy — check that you launched it via `pine windsurf`
(not by clicking the Dock icon afterward).

### Some Windsurf features broke

Most likely an extension or service uses **certificate pinning** and won't
accept our forged cert no matter what. Workarounds:

- Run with `--passthrough-only-logged=false` (the default) so non-tapped
  hosts are blind-tunneled untouched. Don't add pinned hosts to your
  `--hosts` list.
- If a *tapped* host is pinned, it'll show TLS errors in the proxy log.
  Remove that host from your filter.

### "I want to start over completely"

```bash
pine ca reset                       # untrust + delete CA files
rm -rf ~/.pine/sessions/             # delete all session logs (optional)
pine ca trust                        # trust the freshly minted CA
```

---

## What `pine windsurf` does under the hood

1. Loads or mints the local root CA at `~/.pine/ca/root.{crt,key}`.
2. Checks the macOS keychain for that CA. Warns if not trusted.
3. Verifies Windsurf isn't already running (single-instance check).
4. Resolves the Windsurf binary path (`which windsurf`, then
   `/Applications/Windsurf.app/Contents/MacOS/Electron`).
5. Binds a local TCP listener on a free port — that's the proxy.
6. Spawns Windsurf with:
   - Chromium switches: `--proxy-server=http://127.0.0.1:<port>`,
     `--proxy-bypass-list=<-loopback>;localhost;127.0.0.1;::1`
   - Env vars: `HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY`,
     `NODE_EXTRA_CA_CERTS`, `SSL_CERT_FILE`,
     `GRPC_DEFAULT_SSL_ROOTS_FILE_PATH`, `PINE_SESSION`
   - In its own UNIX process group so we can SIGKILL the whole tree
     (Electron forks zygote / GPU / utility / renderer processes).
7. For each TLS connection from Windsurf:
   - If the host matches `--hosts` (or the default Windsurf list), we
     terminate TLS, read the request, forward it upstream, log everything
     to `traffic.jsonl`, and return the response.
   - Otherwise we blind-tunnel — just pipe bytes through, log the CONNECT
     line only.
8. Tracks Windsurf's child PID. Ctrl-C cycle:
   - 1st Ctrl-C: shell delivers SIGINT to the foreground group; we print
     a hint and keep waiting.
   - 2nd Ctrl-C: SIGTERM to Windsurf.
   - 3rd Ctrl-C: SIGKILL the whole process group and force-exit.

---

## Other platforms

**Linux:** `pine ca trust` will print the exact `certutil` and
`update-ca-certificates` commands you need to run yourself — automatic
install isn't wired up yet because there are two trust stores (NSS for
Chromium, system store for everything else) and we'd rather you make the
choice explicitly. Everything else (`pine windsurf`, etc.) works.

**Windows:** ditto — `pine ca trust` prints the `certutil -user
-addstore Root` command. Manual one-time step.

If you'd like full automation on either platform, open an issue.

---

## TL;DR

```bash
./install.sh                    # build & install pine
pine ca trust              # one-time keychain setup
# (quit Windsurf with ⌘Q)
pine windsurf              # launch Windsurf, capture everything
# ... use Windsurf normally ...
# ⌘Q to quit; logs at ~/.pine/last-session
```
