# WebSocket capture — plan

Goal: log every WebSocket frame (text + binary, both directions) into the
session directory the same way `traffic.jsonl` already logs HTTP request /
response pairs, so the TUI can replay an entire WS conversation alongside
the HTTP traffic.

This document is **plan only** — implementation comes after the proto-decoder
work is settled. The TUI-side `↑` marker for upgrade attempts already landed
so we can already *find* the failed upgrades.

---

## 1. Current state (verified, not assumed)

Run from the repo root:

```sh
python workspace/inspect_traffic.py <SESSION_ID>
```

For session `01KRGD7RJ6H7JQBJPS3YR6TW44` (a typical Windsurf chat session):

| signal | count |
|---|---|
| total entries | 2,515 |
| `is_stream: true` (chunked HTTP) | **0** |
| response status `101` | **0** |
| request had `Sec-WebSocket-Key` | **7** |

All 7 upgrade attempts target one URL:

```
GET wss://app.devin.ai/api/acp/live?token=<JWT>
Sec-WebSocket-Version: 13
Sec-WebSocket-Extensions: permessage-deflate; client_max_window_bits
```

Every one returns **`404`** because pine currently strips
`Connection`/`Upgrade` from the forwarded request (per the hop-by-hop
strip list at `src/proxy.rs:657-674`). Devin's edge sees a plain GET on a
WS-only endpoint and rejects it. The 7 entries are the Cascade client's
exponential-backoff retries.

**This is the assistant message stream.** Once we stop breaking the
upgrade, the actual chat tokens will flow through it.

### Endpoint inventory — what actually upgrades

| host | path pattern | role |
|---|---|---|
| `app.devin.ai` | `/api/acp/live?token=…` | **ACP live channel — Cascade chat tokens** |
| (none others observed) | | |

A second pass should look at sessions captured during a long Cascade
turn — Windsurf might also open WS to:

- `wss://server.self-serve.windsurf.com/...` (currently only HTTP/Connect-RPC)
- `wss://server.codeium.com/...`
- Inference-side hosts (`inference.codeium.com`)

The inspector script `workspace/inspect_traffic.py` will catch any of
these as soon as they show up — it keys off `Sec-WebSocket-Key`, not the
Upgrade header that we currently strip.

---

## 2. Why WS is broken right now

`src/proxy.rs::strip_hop_by_hop` removes these headers from every
forwarded request:

```rust
"connection", "proxy-connection", "keep-alive", "transfer-encoding",
"te", "trailer", "upgrade", "proxy-authenticate", "proxy-authorization",
```

Per RFC 7230 §6.1 these are hop-by-hop and intermediaries are *supposed* to
drop them — but the WebSocket handshake (RFC 6455 §4) is the one place
where `Connection: Upgrade` and `Upgrade: websocket` MUST traverse the
proxy. So the current behaviour is technically RFC-compliant for plain
HTTP but breaks WS by default.

Hyper's `serve_connection_with_upgrades` (`src/proxy.rs:151`) is enabled,
so the framework can actually carry an upgrade — we just need to (a) not
strip the headers when the request is a WS handshake, and (b) wire up
post-101 byte tunneling with frame logging in the middle.

---

## 3. Phased implementation

Three phases. Each one is independently shippable; the marker the TUI
already shows (`↑`) gets progressively more accurate as we go.

### Phase A — let the handshake succeed (cheap)

1. In the request handler, **before** `strip_hop_by_hop`, detect a WS
   upgrade by checking:
   - method is `GET`
   - `Connection` header (case-insensitively) contains the token `upgrade`
   - `Upgrade` header equals `websocket` (case-insensitive)
   - `Sec-WebSocket-Key` is present
   - `Sec-WebSocket-Version` is `13`
2. For WS-handshake requests:
   - Skip `strip_hop_by_hop` for the `Connection`, `Upgrade`,
     `Sec-WebSocket-*` headers (preserve them on the upstream request).
   - Forward as usual.
3. When the upstream response is `101 Switching Protocols`:
   - Forward `101` + `Upgrade`, `Connection`, `Sec-WebSocket-Accept`,
     `Sec-WebSocket-Extensions`, `Sec-WebSocket-Protocol` back to the
     client unchanged.
   - **Do not** terminate the response body — there isn't one.
4. After the response is sent, await `hyper::upgrade::on(&mut req)` (the
   downstream side) and `hyper::upgrade::on(&mut resp)` (the upstream
   side) to get two raw `Upgraded` byte streams.
5. For Phase A only, just `tokio::io::copy_bidirectional` between them.
   No frame decode yet — but the upgrade succeeds and Cascade chat
   actually works through pine.

After Phase A: in the TUI, all 7 of those `↑` 404s become `↑` 101s, the
real session works, and we can move on to actually capturing payload.

### Phase B — log frames (the real win)

Replace the `copy_bidirectional` from Phase A with a framed reader on
each side.

**Library**: `fastwebsockets` (no async runtime opinions, MIT license,
already used by Cloudflare). Alternative: hand-roll RFC-6455 frame
parsing — it's ~150 LOC and avoids a dependency. Decision: **roll our
own** since we're a logger, not a client/server, and the RFC is small.

**Storage**: each WS connection gets one file:

```
sessions/<sid>/stream/<req-id>.ws.jsonl
```

One JSON object per frame, append-only, one line each:

```json
{"ts":"2026-05-13T10:13:42.001234Z","dir":"c2s","op":"text","fin":true,"len":238,"payload":"…"}
{"ts":"2026-05-13T10:13:42.014567Z","dir":"s2c","op":"binary","fin":true,"len":1024,"payload_b64":"…"}
{"ts":"2026-05-13T10:13:42.020112Z","dir":"c2s","op":"ping","fin":true,"len":0}
{"ts":"2026-05-13T10:13:42.020910Z","dir":"s2c","op":"pong","fin":true,"len":0}
{"ts":"2026-05-13T10:13:55.500000Z","dir":"s2c","op":"close","fin":true,"len":2,"code":1000,"reason":""}
```

Fields:

| field | meaning |
|---|---|
| `ts` | RFC 3339 microsecond UTC timestamp |
| `dir` | `c2s` (downstream → upstream) or `s2c` (upstream → downstream) |
| `op` | `text`, `binary`, `ping`, `pong`, `close`, `cont` |
| `fin` | RFC 6455 FIN bit (false = fragmented, more frames coming) |
| `len` | uncompressed payload length |
| `payload` | UTF-8 text (only when `op == "text"`) |
| `payload_b64` | base64 (binary, ping/pong with payload, close with reason) |
| `rsv1/2/3` | reserved bits, included only when non-zero (e.g. permessage-deflate) |

The proxy must NOT mask frames going server→client (servers don't mask)
and must NOT unmask client→server frames before forwarding (the upstream
expects them masked). Logging happens *between* parsing and re-emitting
the same byte sequence, so the wire is unchanged.

**Update `ResponsePayload` in `src/log.rs`**:

```rust
pub struct ResponsePayload {
    ...
    pub is_stream: bool,
    pub stream_path: Option<String>,
    pub is_websocket: bool,        // new
    pub ws_path: Option<String>,   // new — points at <id>.ws.jsonl
}
```

`stream_path` already exists for SSE; `ws_path` keeps WS distinct so the
TUI can pick the right renderer.

**Update `traffic.jsonl` writer**: emit one record per upgraded
connection at handshake time (status 101, body_size 0), then continue
appending frames to the `.ws.jsonl` file until the close frame or socket
shutdown. The `traffic.jsonl` row is finalized when the WS closes;
`response.body_size` becomes the total bytes of the framed payload (sum
of all frame `len`s, both directions, for back-of-envelope display).

### Phase C — permessage-deflate (the messy bit)

Devin's handshake explicitly negotiates
`Sec-WebSocket-Extensions: permessage-deflate; client_max_window_bits`,
so frames are **per-message DEFLATE compressed** with a shared sliding
window (RFC 7692). Without this, every frame in Phase B will look like
random binary.

Requirements:

1. Parse the `Sec-WebSocket-Extensions` value from the 101 response to
   know which side requested `client_no_context_takeover` /
   `server_no_context_takeover` and the window-bits.
2. Per direction, keep a `flate2::Decompress` (or
   `miniz_oxide::inflate::stream`) state initialized with raw DEFLATE
   (no zlib header).
3. For each data frame with the RSV1 bit set:
   - Append the payload bytes to a buffer until FIN.
   - On FIN: append the trailing `0x00 0x00 0xFF 0xFF` block (RFC 7692
     §7.2.2) and feed through the decompressor.
   - If `*_no_context_takeover` was negotiated for that direction, reset
     the decompressor between messages; otherwise keep state across
     messages.
4. Write the **decompressed** payload to `payload`/`payload_b64` and
   record `compressed_len` separately so we can show compression ratio
   in the TUI.

Re-emitting frames upstream/downstream still uses the **original
compressed bytes** — we only decompress for the log.

### Phase D — TUI rendering (small)

In `packages/tui/screens/detail.py`, when an entry has
`response.is_websocket`:

- Header: show `WS UPGRADED — <peer>` + extensions + subprotocol.
- Body: stream the `.ws.jsonl` file the same way SSE is streamed today,
  but with arrow icons:
  - `→` `c2s text` `len=238` followed by indented payload
  - `←` `s2c binary` `len=1024` followed by `[hex preview…]` or
    decoded JSON if the payload `json.loads` cleanly.
- Bindings: `t` to filter to text frames only, `b` for binary,
  `c` to copy a frame's payload to clipboard.

The `↑` marker on the request list already lights up for these entries
once Phase A lands and the response status flips to 101.

---

## 4. Risks & open questions

1. **HTTP/2 WS (RFC 8441 / extended CONNECT)**. Hyper supports H/2 but
   `:protocol = websocket` extended CONNECT is a separate code path. If
   any host (Codeium edge does this) negotiates h2 ALPN and uses
   extended CONNECT, the Phase A check fails. **Mitigation**: also
   detect `:method = CONNECT` + `:protocol = websocket` pseudo-header
   pair on H/2 streams. Defer to Phase B+ if h2-WS shows up.
2. **Subprotocols**. ACP/Cascade may use a custom WS subprotocol on top
   of `permessage-deflate`. The frame layer sees opaque bytes; semantic
   decoding (e.g. ACP message envelopes → fields) is out of scope here
   but enabled by Phase B output.
3. **Large frames**. RFC 6455 allows 64-bit lengths. Capping payload
   inline at 64 KiB (matching the existing `INLINE_BODY_LIMIT`) and
   spilling to `bodies/<id>.f<seq>.bin` keeps the JSONL line length sane.
4. **Half-closed / abruptly-closed**. If the client process is killed
   mid-stream (which Cascade's flush does), we still want a partial log.
   Use `tokio::select!` over both directions and flush on shutdown.
5. **Client cert / mTLS to upstream**. Today the proxy uses a single
   leaf cert. For Devin specifically, the WS endpoint is
   `app.devin.ai` over a normal cert chain — no mTLS. Should be fine.

---

## 5. Concrete TODO list (when we start building)

```
[A1]  src/proxy.rs   detect WS handshake before hop-by-hop strip
[A2]  src/proxy.rs   preserve Upgrade/Connection/Sec-WebSocket-* headers on WS handshake
[A3]  src/proxy.rs   forward 101 + handshake response headers verbatim
[A4]  src/proxy.rs   on 101: hyper::upgrade::on() both sides, copy_bidirectional
[A5]  src/log.rs     mark request_id as ws=true at handshake time
[A6]  test           connect a tiny WS server in tests/, hit it via proxy, assert handshake succeeds

[B1]  src/ws.rs      RFC 6455 frame reader/writer (no compression)
[B2]  src/log.rs     add is_websocket / ws_path fields, write helpers
[B3]  src/proxy.rs   replace copy_bidirectional with framed reader → log → framed writer
[B4]  test           round-trip text + binary + ping + close through proxy, assert .ws.jsonl matches

[C1]  src/ws_deflate.rs   permessage-deflate negotiation + per-direction state
[C2]  src/ws.rs           hook decompressor into the read path; keep compressed bytes for re-emit
[C3]  test                replay a captured Devin ACP frame; assert decompresses to JSON

[D1]  packages/tui/screens/detail.py   WS renderer
[D2]  packages/tui/screens/detail.py   filter + copy bindings (t/b/c)
[D3]  workspace/test_ws_render.py      smoke test on a fixture .ws.jsonl
```

Estimated effort:
- Phase A: ~200 LOC, half a day
- Phase B: ~400 LOC, one day
- Phase C: ~300 LOC + careful testing, one day
- Phase D: ~150 LOC, half a day

Total ~3 days of focused work to land a complete Cascade-stream-grade
WebSocket logger.
