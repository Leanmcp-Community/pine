# pine — Plan

A small Rust tool that captures every HTTPS request/response Claude Code makes
to the Anthropic API — full URL, method, headers, body, status, response
headers, response body (incl. SSE) — and writes them to `~/.pine/` as
plaintext logs, *before* they hit the wire encrypted (and after they come back
decrypted).

---

## 1. The constraint that decides the architecture

`which claude` on this machine →
`/Users/ddod/.nvm/versions/node/v24.11.1/lib/node_modules/@anthropic-ai/claude-code/bin/claude.exe`.

It is **not** a Node.js script. It is a **Bun-compiled Mach-O arm64 binary**
(the `__BUN` segment is present), signed with the **hardened runtime** by
`Developer ID Application: Anthropic PBC (Q6L2SF6YDW)`:

```
CodeDirectory ... flags=0x10000(runtime)
```

Two consequences fall out of this:

1. **`NODE_OPTIONS=--require ./hook.js` is irrelevant** — this is not Node, and
   Bun's compiled-binary mode does not honor an arbitrary preload script.
2. **`DYLD_INSERT_LIBRARIES` is ignored.** macOS dyld silently drops
   `DYLD_INSERT_LIBRARIES` for hardened-runtime processes unless the loaded
   dylib is signed by the same Team ID *or* the binary carries
   `com.apple.security.cs.allow-dyld-environment-variables` +
   `com.apple.security.cs.disable-library-validation`. We cannot satisfy
   either without re-signing `claude.exe`, which would invalidate the
   distribution signature.

On top of that, Bun statically links BoringSSL into the runtime, so even if
injection worked, there is no exported `SSL_write` symbol on the dyld boundary
to interpose against — it's all internal.

Conclusion: any approach that tries to hook *inside* the Claude process is a
dead end on macOS. We have to intercept at the network boundary.

## 2. Approaches considered

| # | Approach | Verdict |
|---|---|---|
| A | `DYLD_INSERT_LIBRARIES` shim hooking `SSL_write`/`SSL_read` | ✗ hardened runtime + statically-linked BoringSSL |
| B | `NODE_OPTIONS` / `--import` preload that monkey-patches `fetch` | ✗ not Node, it's a compiled Bun binary |
| C | DTrace / `dtruss` on TLS internals | ✗ SIP-restricted, brittle, no plaintext anyway |
| D | eBPF uprobes on TLS lib | ✗ Linux-only, doesn't apply |
| E | **Local HTTPS MITM proxy + `HTTPS_PROXY` env var** | ✓ runtime-agnostic, full plaintext, fully reversible, no re-signing |
| F | Transparent NFQUEUE / pf redirect to a local proxy | Equivalent visibility to E but needs root + pf rules — overkill |

**Approach E wins.** Bun's `fetch` (which the Anthropic SDK uses) honors the
standard `HTTPS_PROXY` / `https_proxy` env var, and a custom CA can be trusted
via `NODE_EXTRA_CA_CERTS` / `SSL_CERT_FILE` / `BUN_CA_BUNDLE` without
modifying the binary or the system trust store.

## 3. What `pine` actually is

A single Rust binary that operates as a **launcher + proxy**:

```
$ pine [-- claude args...]
```

When invoked it:

1. Ensures `~/.pine/` exists and a per-user CA exists at
   `~/.pine/ca/{root.crt,root.key}` (created on first run, 4096-bit RSA
   or P-256, ~10 yr validity, marked CA:TRUE, key stays mode 0600).
2. Starts a local HTTPS MITM proxy on `127.0.0.1:<random-free-port>`.
3. Forks/execs `claude` (resolved via `$PATH`, or `--claude-bin`) with a
   patched env:
   ```
   HTTPS_PROXY=http://127.0.0.1:<port>
   HTTP_PROXY=http://127.0.0.1:<port>
   ALL_PROXY=http://127.0.0.1:<port>
   NO_PROXY=localhost,127.0.0.1     # don't proxy ourselves recursively
   NODE_EXTRA_CA_CERTS=~/.pine/ca/root.crt
   SSL_CERT_FILE=~/.pine/ca/root.crt
   BUN_CA_BUNDLE=~/.pine/ca/root.crt
   PINE_SESSION=<ulid>
   ```
4. Streams every request through the proxy, logging only the configured
   target hosts (default: `api.anthropic.com`, `*.anthropic.com`,
   `statsig.anthropic.com`, `console.anthropic.com`). Anything else can be
   passed through without logging or, with `--passthrough-only-logged`,
   refused.
5. On Claude exit, flushes logs and exits with the same code.

**Why this is "minimalist":** no system-wide proxy, no system trust changes,
no kernel modules, no entitlements, no daemon. The CA cert lives entirely
under `~/.pine/`. Stop using `pine` and nothing about your system
has changed.

## 4. Why Rust

- TLS termination on both legs (downstream-from-claude *and* upstream-to-API)
  in pure-Rust stacks (`rustls`) gives a single static binary with no OpenSSL
  dependency drama on macOS.
- `hyper` 1.x + `rustls` + `tokio` is a battle-tested combo for exactly this
  shape of proxy.
- A statically-linked binary distributes cleanly (no node\_modules, no Python
  venv, one `cargo install --path .`).
- Concurrency model fits SSE / long-lived streaming responses naturally.

Reasonable alternatives if not Rust: **Go** (`net/http` + `crypto/tls`, very
similar story, simpler to write but bigger binary and no `rustls` ALPN
ergonomics) or **Bun** itself (eat-your-own-dogfood, but you pay an extra
runtime + we'd be debugging a Bun proxy in the same family of stack as the
target). Rust is the right pick here.

## 5. Project layout

```
pine/
├── Cargo.toml
├── Cargo.lock
├── .gitignore
├── plan.md
├── README.md                  # written later, not in v0.1
├── src/
│   ├── main.rs                # CLI parsing + orchestration
│   ├── ca.rs                  # CA bootstrap + leaf cert minting
│   ├── proxy.rs               # MITM HTTPS proxy core
│   ├── intercept.rs           # request/response capture + filtering
│   ├── log.rs                 # JSONL writer, file rotation, redaction
│   ├── launcher.rs            # spawn claude with patched env
│   └── api.rs                 # optional REST query server (~/.pine/api)
└── tests/
    └── e2e.rs                 # spin up proxy, hit it with reqwest, verify log
```

## 6. Crate dependencies (intent — versions chosen at `cargo new` time)

Core:
- `tokio` (full features)
- `hyper` 1.x — server + client
- `hyper-util` — `TokioExecutor`, `TokioIo`, legacy client
- `tokio-rustls` + `rustls` + `rustls-pemfile` — TLS both legs
- `rustls-native-certs` — trust the OS roots when talking *to* api.anthropic.com
- `rcgen` — generate CA + per-host leaf certs on the fly
- `bytes`, `http`, `http-body-util`

CLI / IO:
- `clap` (derive)
- `serde`, `serde_json`
- `dirs` — locate `~/.pine`
- `ulid` — session + log ids
- `time` — RFC3339 timestamps
- `tracing` + `tracing-subscriber` — internal diagnostics (separate from
  captured-traffic logs)

Optional:
- `axum` + `tower-http` — minimal REST API for `pine logs ls/show/tail`
  (the "REST-based" surface)
- `eventsource-stream` — SSE-aware logging so streaming responses get logged
  event-by-event rather than as one opaque blob

## 7. MITM proxy: how the bytes actually flow

Forward proxy mode (what `HTTPS_PROXY` triggers):

1. Claude opens TCP to `127.0.0.1:<port>`, sends
   `CONNECT api.anthropic.com:443 HTTP/1.1`.
2. Proxy replies `200 Connection Established`.
3. Proxy mints a leaf cert for `api.anthropic.com` signed by our CA on the
   fly (`rcgen`, cached in memory keyed by SNI hostname), then performs a
   TLS handshake **against Claude** using that leaf cert. Claude trusts it
   because we set `NODE_EXTRA_CA_CERTS`/`BUN_CA_BUNDLE`.
4. Concurrently the proxy opens a fresh TLS connection upstream to the real
   `api.anthropic.com:443`, validating against `rustls-native-certs` (the
   real OS trust store).
5. Hyper drives HTTP/1.1 *or* HTTP/2 on each leg via ALPN; we negotiate the
   downstream ALPN to match what we negotiated upstream so streaming
   semantics are preserved.
6. Each request and its response are **observed in cleartext on the proxy**,
   captured into a log record, then forwarded.

Streaming (Anthropic's `text/event-stream` responses) is logged event by
event using `eventsource-stream` so you see deltas live in the log file,
not just after the stream closes.

## 8. CA + trust handling

- CA created on first run. Files: `~/.pine/ca/root.crt`,
  `~/.pine/ca/root.key` (mode 0600).
- Trust is **per-process via env vars**, not system-wide. We never call
  `security add-trusted-cert`. The CA exists only inside this user's home
  dir; any process that doesn't read those env vars won't trust it.
- `pine ca path` prints the cert path. `pine ca rotate` regenerates.
- Leaf certs minted on demand for each upstream host, cached in-process.

This is important: a CA on disk is sensitive. Refuse to start if
`~/.pine/ca/root.key` is mode > 0600 or owned by another user. Document
this prominently.

## 9. Log format

Per-session directory:

```
~/.pine/sessions/<ulid>/
  meta.json          # session start ts, claude argv, claude version, pid
  traffic.jsonl      # one JSON object per request/response pair
  bodies/<req-id>.req.bin
  bodies/<req-id>.res.bin
  stream/<req-id>.sse.jsonl   # SSE events, in order, only if streaming
```

Each `traffic.jsonl` line:

```json
{
  "id": "01HXYZ...",
  "ts_start": "2026-05-07T12:34:56.789Z",
  "ts_end":   "2026-05-07T12:34:57.123Z",
  "method": "POST",
  "url": "https://api.anthropic.com/v1/messages",
  "http_version": "HTTP/2",
  "request": {
    "headers": [["x-api-key","<redacted-or-raw>"], ...],
    "body_path": "bodies/01HXYZ.req.bin",
    "body_size": 18342,
    "body_inline": null
  },
  "response": {
    "status": 200,
    "headers": [...],
    "body_path": "bodies/01HXYZ.res.bin",
    "body_size": 0,
    "stream_path": "stream/01HXYZ.sse.jsonl",
    "is_stream": true
  }
}
```

Bodies under ~64 KB get inlined as `body_inline` (utf-8 if valid, else
base64); larger bodies go to a file. SSE responses always go to
`stream/<id>.sse.jsonl`.

**Redaction policy** (default on, `--no-redact` to disable):
- `authorization`, `x-api-key`, `cookie`, `set-cookie`, `proxy-authorization`
  → replaced with `"<redacted:sha256:abcdef…>"` so you can correlate without
  leaking the secret. The user explicitly asked for "everything", so
  `--no-redact` exists, but the default is safe.

## 10. CLI surface

```
pine                       # launch claude under tap with defaults
pine -- --print "hello"    # everything after -- forwarded to claude
pine --hosts api.anthropic.com,statsig.anthropic.com
pine --no-redact
pine --port 0              # 0 = pick free port (default)
pine --passthrough-only-logged
pine ca path | rotate | print
pine logs ls               # list sessions
pine logs show <session>   # pretty-print traffic.jsonl
pine logs tail [<session>] # follow live
pine api [--port N]        # start the REST query server (optional)
```

## 11. Optional REST query API (the "rest-based" piece, if that's what you meant)

`pine api` starts an `axum` server on `127.0.0.1:<port>`:

- `GET /sessions` → list session ids + meta
- `GET /sessions/:id` → session meta + traffic index
- `GET /sessions/:id/traffic` → paginated traffic.jsonl
- `GET /sessions/:id/traffic/:reqid` → full record
- `GET /sessions/:id/traffic/:reqid/req-body` → raw request body
- `GET /sessions/:id/traffic/:reqid/res-body` → raw response body
- `GET /sessions/:id/traffic/:reqid/stream` → SSE replay of captured stream
- `GET /healthz`

Bound to loopback only, no auth. Off by default; user starts it explicitly.

## 12. Edge cases / things that will bite us if we don't plan for them

- **HTTP/2 on both legs.** Anthropic API negotiates h2. The downstream leg
  must also offer h2 via ALPN, otherwise Claude may behave differently than
  in production. Hyper 1.x supports this; we just have to wire it up.
- **Streaming responses.** SSE keep-alives, large transfers, mid-stream
  errors. Don't buffer the response before forwarding — pipe and tee.
- **Connection reuse.** Claude will keep the proxy connection alive and
  multiplex requests over it (especially with h2). Each h2 stream is a
  separate log record.
- **Bun fetch + proxies.** Verify Bun honors `HTTPS_PROXY` for h2 upstreams
  via `CONNECT`. If not, fall back to forcing h1 upstream and let the proxy
  re-negotiate h2 to api.anthropic.com (we already terminate TLS, so the
  Claude-side protocol is independent of the upstream-side protocol).
- **Statsig / telemetry hosts.** Claude Code talks to multiple hosts. Decide
  whether to log them. Default: log everything `*.anthropic.com`,
  pass-through everything else silently.
- **Process tree.** If `claude` shells out, the children inherit `HTTPS_PROXY`
  and that's fine. If they don't honor it, we miss them — document this.
- **Disk usage.** Long sessions with big bodies = lots of bytes. Add
  `--max-session-bytes` with a safe default (e.g. 1 GiB) and rotate / refuse
  past that.
- **CA private key safety.** Hard-fail on bad perms. Never log the key path.
  Recommend users avoid syncing `~/.pine/ca/` to cloud storage.
- **Self-loop.** Make sure `NO_PROXY=localhost,127.0.0.1` is set so the
  proxy's own outbound connection doesn't go through itself.
- **Hardened-runtime gotcha (already known).** Don't even try to use
  `DYLD_INSERT_LIBRARIES`. Document it so future-you doesn't waste a day on
  it.

## 13. Implementation milestones

- **M0 — scaffolding (this PR):** `plan.md`, `.gitignore`, empty
  `Cargo.toml`. No code yet.
- **M1 — CA + leaf minting:** `pine ca path|print|rotate`. Persisted
  CA, deterministic leaf generation, cache by SNI.
- **M2 — vanilla forward proxy (no MITM):** accept `CONNECT`, blind-tunnel
  upstream, no logging. Verifies `claude` actually uses it.
- **M3 — MITM HTTPS, HTTP/1.1 only, request-only logging:** terminate TLS
  using a leaf, forward, log method/url/req-headers/req-body to JSONL.
- **M4 — response logging incl. SSE streaming:** tee response body, parse
  `text/event-stream` into `stream/*.sse.jsonl`.
- **M5 — HTTP/2 on both legs.**
- **M6 — host filtering + redaction defaults.**
- **M7 — `pine logs ls/show/tail` CLI.**
- **M8 — `pine api` REST surface (optional).**
- **M9 — packaging + README.**

## 14. Open questions for you

1. **REST vs Rust.** Re-reading your message I'm ~95% sure you typed
   "rest-based" but meant "rust-based". The plan above goes Rust. The REST
   query API in §11 is a small bonus — we can skip it if you don't want it.
2. **Scope of capture.** Just `api.anthropic.com`? Or every domain Claude
   Code touches (Statsig, GitHub, npm registry on updates, etc.)? Default
   above: only `*.anthropic.com`.
3. **Redaction default.** You said "everything". Are you OK with the default
   being to hash the auth header and require `--no-redact` to see it raw?
4. **System trust.** Confirmed plan: per-process trust via env vars only,
   no `security add-trusted-cert`. Push back if you'd rather have system
   trust.
5. **Auto-launch.** Should `pine` with no args launch `claude`, or
   should it just start the proxy and print the env vars for you to source?
   I picked auto-launch above.
