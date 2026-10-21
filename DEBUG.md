# Debugging `pine windsurf`

When something doesn't work, this is the playbook. Most issues fall into one of
three buckets: **Windsurf exits immediately**, **TLS errors / "certificate
authority invalid"**, or **traffic isn't being captured**.

---

## Symptom 1 — Windsurf exits immediately, pine returns to your shell prompt

> "When I open Windsurf, pine immediately ends on the terminal."

### Why this happens

Windsurf is an Electron / Chromium app. Electron has a **single-instance
lock**: when you launch `/Applications/Windsurf.app/Contents/MacOS/Electron`,
it checks a lock file in the user data dir. If *any* other Windsurf process is
alive — including hidden helpers or a window you "closed" but didn't `⌘Q` —
the new process:

1. Hands its CLI args to the existing Windsurf via IPC,
2. **Exits immediately with code 0.**

Our supervisor sees the child exit and shuts down. The Windsurf window you
see open is the *original* one, which never received our `--proxy-server`
switch or `NODE_EXTRA_CA_CERTS` env, so it bypasses pine entirely.

`is_windsurf_running()` in pine catches the obvious case via
`pgrep -x Windsurf`, but background helpers and renamed processes can slip
past it.

### Fix A — fully kill every Windsurf process

```bash
# See what's still alive
pgrep -fl -i windsurf

# Nuke everything (force; Electron helpers ignore plain SIGTERM sometimes)
pkill -9 -i windsurf
sleep 1
pgrep -fl -i windsurf      # should print nothing now

# Re-launch
pine windsurf
```

If `pgrep` still finds Windsurf processes after `pkill -9`, check Activity
Monitor — there may be a "Windsurf Helper (GPU)" or "Windsurf Helper
(Renderer)" you can quit by hand.

### Fix B (recommended) — isolated profile

Bypasses the singleton lock entirely. Different `--user-data-dir` = different
lock file = fresh Electron instance, even if your real Windsurf is still
running.

```bash
mkdir -p /tmp/wsf-tap
pine windsurf --user-data-dir /tmp/wsf-tap
```

Sign in once and that profile dir remembers next time you reuse it. You can
keep your daily-driver Windsurf running normally in parallel.

### How pine helps you spot this

If the child exits in under 3 seconds with code 0, pine prints:

```
pine: ⚠  windsurf exited almost immediately. Most likely cause:
              another Windsurf process is alive and the new launch was
              forwarded to it via Electron's single-instance lock ...
```

If you see that hint, you're in this bucket. Apply Fix A or B.

---

## Symptom 2 — Windsurf opens but every request fails with "ERR_CERT_AUTHORITY_INVALID"

### Why

Chromium uses the **OS trust store**, not `SSL_CERT_FILE` / `NODE_EXTRA_CA_CERTS`.
For Windsurf to accept the leaf certs pine mints on the fly, the
pine root CA must be installed in your macOS login keychain.

### Fix

```bash
pine ca trust         # one-time; prompts for your login password

# Verify
security find-certificate -c "pine local root" >/dev/null && echo OK

# Or just ask pine
pine ca path          # prints CA path
pine windsurf         # banner shows "root CA is trusted ✓" or a warning
```

If the trust banner says ✓ but you still get cert errors, your CA file may be
out of sync with what's in the keychain (this happens after `ca reset` if you
forgot to re-trust). Clean slate:

```bash
pine ca reset         # untrust + delete on-disk CA files
pine ca trust         # mint a fresh one and trust it
pine windsurf
```

---

## Symptom 3 — pine is running but `traffic.jsonl` is empty (or only has one or two entries)

### Diagnose first — is Windsurf even using the proxy?

The banner shows the proxy URL, e.g. `http://127.0.0.1:54321`. While Windsurf
is open, in another terminal:

```bash
lsof -nP -i :54321 | head    # any connections to that port?
```

If nothing is connected → Windsurf isn't going through the proxy. Most likely
the singleton-lock issue from §1 (you're talking to the wrong Windsurf).

If connections exist but `traffic.jsonl` is sparse → host filter is too narrow.

### Verbose pine logs

See every CONNECT, every TLS handshake, every body decision:

```bash
PINE_LOG=pine=debug pine windsurf --user-data-dir /tmp/wsf-tap
```

You'll see lines like:

```
DEBUG mitm host=server.codeium.com sni=server.codeium.com
DEBUG tunnel host=updates.windsurf.com  (not in --hosts; blind-tunneled)
```

Anything labeled `tunnel` is **not** captured in detail — only the CONNECT is
recorded. To capture it, add the host to `--hosts` (or use `--hosts '*'` for
total capture).

### Verbose Windsurf / Chromium logs

Pass Chromium switches after `--`:

```bash
pine windsurf --user-data-dir /tmp/wsf-tap -- \
  --enable-logging=stderr --v=1 \
  --log-net-log=/tmp/wsf-net.json
```

`--log-net-log` writes a structured JSON event log of every Chromium network
operation. Open `/tmp/wsf-net.json` in <https://netlog-viewer.appspot.com/>
to see exactly which requests Windsurf made, what cert errors fired, what
proxy decisions Chromium took, and which requests the renderer initiated vs.
which the network service did.

This is the single best tool for "why is Chromium ignoring my proxy" or "why
does this one request fail."

---

## One-time discovery run — capture *everything*

When you don't know which hosts Windsurf actually contacts, MITM all of them
once, harvest the list, then narrow back down.

```bash
pine windsurf --user-data-dir /tmp/wsf-tap --hosts '*'
```

`*` (quoted, so the shell doesn't glob-expand it) matches every host. After
the session:

```bash
SESSION_DIR=$(awk 'NR==2' ~/.pine/last-session)

# All unique hosts seen
jq -r '.url' "$SESSION_DIR/traffic.jsonl" \
  | sed -E 's|https?://([^/]+).*|\1|' | sort -u

# Just the ones that returned 200
jq -r 'select(.response.status == 200) | .url' "$SESSION_DIR/traffic.jsonl" \
  | sed -E 's|https?://([^/]+).*|\1|' | sort -u

# Hosts that we blind-tunneled (CONNECT-only, no body capture)
jq -r 'select(.method == "CONNECT") | .url' "$SESSION_DIR/traffic.jsonl" \
  | sort -u
```

Use the resulting list to update `WINDSURF_HOSTS` in `src/main.rs` to a
narrow, well-justified set, then rebuild.

---

## Less common gotchas

### "Windsurf is already running" but I just quit it

macOS's "App Nap" or a stuck helper can keep a Windsurf process alive after
the dock icon disappears. Always verify with:

```bash
pgrep -fl -i windsurf
```

If it returns anything, you're not actually quit. `pkill -9 -i windsurf`
clears it.

### Cert pinning on a specific host

Some services (often gRPC clients, sometimes auto-update endpoints) **pin
certificates** in code. Even with our CA trusted, the pinned client refuses
our forged leaf cert. Symptoms: that one feature breaks while everything
else works; you'll see TLS handshake errors in
`PINE_LOG=pine=debug` output.

Workaround: don't tap that host. Remove it from `--hosts` (or from
`WINDSURF_HOSTS`); pine will blind-tunnel it untouched and the feature
will work, you just won't get bodies.

### Wrong binary path

Some installs put the actual Electron binary at
`/Applications/Windsurf.app/Contents/MacOS/Windsurf` instead of
`.../Electron`. pine searches both, but if you have a custom location:

```bash
pine windsurf --windsurf-bin /path/to/your/windsurf-binary
```

### Network interface changed mid-session (VPN connect/disconnect)

The proxy listens on `127.0.0.1`, so loopback is unaffected. But Windsurf's
connection pool may hold dead sockets after a VPN flap. Quit Windsurf,
restart pine.

---

## Full clean-slate recipe

When in doubt, blow everything away and start over:

```bash
# 1. Kill all Windsurf
pkill -9 -i windsurf
sleep 1
pgrep -fl -i windsurf       # must be empty

# 2. Reset the CA
pine ca reset
pine ca trust

# 3. (optional) Nuke old session logs
rm -rf ~/.pine/sessions/

# 4. Fresh isolated profile
rm -rf /tmp/wsf-tap
mkdir -p /tmp/wsf-tap

# 5. Launch with full diagnostics
PINE_LOG=pine=debug pine windsurf --user-data-dir /tmp/wsf-tap -- \
  --enable-logging=stderr --v=1 --log-net-log=/tmp/wsf-net.json
```

If *that* doesn't work, capture and share:

- The full pine stderr (everything from launch to exit).
- Output of `pgrep -fl -i windsurf` taken **right before** the launch.
- The first few entries of `traffic.jsonl`:
  `head -5 ~/.pine/sessions/$(awk 'NR==1' ~/.pine/last-session)/traffic.jsonl`
- The first ~50 net-log events:
  `jq '.events[:50]' /tmp/wsf-net.json`

---

## Quick reference

| Goal | Command |
|---|---|
| Verbose pine logs | `PINE_LOG=pine=debug pine windsurf` |
| Verbose Chromium logs | `pine windsurf -- --enable-logging=stderr --v=1` |
| Chromium net-log file | `pine windsurf -- --log-net-log=/tmp/wsf-net.json` |
| Isolated profile | `pine windsurf --user-data-dir /tmp/wsf-tap` |
| Capture every host once | `pine windsurf --hosts '*'` |
| Skip CA trust check | `pine windsurf --skip-trust-check` |
| Force-kill all Windsurf | `pkill -9 -i windsurf` |
| Reset CA from scratch | `pine ca reset && pine ca trust` |
| Find latest session dir | `awk 'NR==2' ~/.pine/last-session` |
