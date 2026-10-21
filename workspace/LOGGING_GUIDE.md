# Windsurf Logging in pine — Complete Guide

## Overview

`pine` is a **man-in-the-middle (MITM) proxy** that captures every HTTPS request/response that Windsurf makes. It logs **everything** by default — nothing is filtered or discarded unless you explicitly exclude hosts.

---

## What Gets Logged?

**Short answer:** Every HTTPS request/response pair that matches your host filter (default: Windsurf-related hosts like `*.codeium.com`, `*.windsurf.com`, `api.anthropic.com`, etc.).

**What's captured:**
- HTTP method (GET, POST, etc.)
- Full URL
- Request headers (with optional redaction of sensitive ones like `Authorization`, `Cookie`)
- Request body (full content)
- Response status code
- Response headers
- Response body (full content)
- Timestamps (start and end)
- Any errors that occurred

**What's NOT captured:**
- HTTPS traffic to hosts outside your `--hosts` filter (unless you use `--hosts '*'` for discovery)
- Blind-tunneled connections (non-MITM hosts are just piped through)

---

## Session Directory Structure

When you run `pine windsurf`, it creates a session under `~/.pine/sessions/<SESSION_ID>/`:

```
~/.pine/sessions/01HXY.../
├── meta.json              # Session metadata (PID, start time, proxy port, host filter, etc.)
├── traffic.jsonl          # One JSON line per request/response pair
├── bodies/                # Large request/response bodies (>64KB)
│   ├── <req-id>.req.bin   # Request body if too large for inline
│   └── <req-id>.res.bin   # Response body if too large for inline
└── stream/                # Streaming responses (SSE, WebSocket)
    ├── <req-id>.sse.jsonl # Server-Sent Events (one JSON line per event)
    └── <req-id>.ws.jsonl  # WebSocket messages
```

---

## The `bodies/` Folder Explained

### Why does it exist?

The `traffic.jsonl` file stores one JSON record per request/response. To keep JSON parsing fast and memory-efficient, **large bodies are spilled to disk** instead of inlined.

### Threshold

Bodies **≤ 64 KB** are inlined directly in `traffic.jsonl` under:
- `request.body_inline.data` (for requests)
- `response.body_inline.data` (for responses)

Bodies **> 64 KB** are written to `bodies/` and referenced by path:
- `request.body_path` → `"bodies/<req-id>.req.bin"`
- `response.body_path` → `"bodies/<req-id>.res.bin"`

### File naming

Each request gets a unique ID (e.g., `abc123def456`). The files are:
- **`.req.bin`** — raw request body bytes
- **`.res.bin`** — raw response body bytes

The `.bin` extension is just a convention; the files are **raw binary data**, not text.

---

## How Python Extracts Bodies

The `tap.py` script (and other workspace tools) read bodies like this:

### 1. Check if body is inline or external

```python
def decode_body(body_inline: dict | None, body_path: str | None, content_type: str):
    if body_inline is None and not body_path:
        return "", "empty"
    
    if body_path:
        # Body is in bodies/ folder — read it
        p = Path(body_path)
        raw = p.read_bytes()
    else:
        # Body is inline in traffic.jsonl
        enc = body_inline.get("encoding", "")  # "utf8" or "base64"
        data_str = body_inline.get("data", "")
        if enc == "base64":
            raw = base64.b64decode(data_str)
        else:
            raw = data_str.encode()
```

### 2. Decompress if gzipped

```python
    if raw[:2] == b"\x1f\x8b":  # gzip magic number
        raw = gzip.decompress(raw)
```

### 3. Parse based on content-type

```python
    ct = content_type.lower()
    
    if "json" in ct:
        obj = json.loads(raw)
        return json.dumps(obj, indent=2), "json"
    
    if "text/" in ct or "html" in ct or "xml" in ct:
        return raw.decode("utf-8", errors="replace"), "text"
    
    # Binary/proto — hex-dump first 512 bytes
    return hex_dump(raw), "binary"
```

---

## Example: Reading a Session in Python

```python
import json
from pathlib import Path

session_dir = Path.home() / ".pine" / "sessions" / "01HXY..."
traffic_file = session_dir / "traffic.jsonl"

for line in traffic_file.read_text().splitlines():
    if not line.strip():
        continue
    
    record = json.loads(line)
    
    # Access request
    req = record["request"]
    req_headers = req["headers"]  # list of [name, value] pairs
    req_body_inline = req.get("body_inline")  # None or {"encoding": "utf8"|"base64", "data": "..."}
    req_body_path = req.get("body_path")      # None or "bodies/abc123.req.bin"
    
    # Access response
    resp = record["response"]
    resp_status = resp["status"]  # e.g., 200
    resp_headers = resp["headers"]
    resp_body_inline = resp.get("body_inline")
    resp_body_path = resp.get("body_path")
    
    # If body is external, read it
    if req_body_path:
        req_body_bytes = (session_dir / req_body_path).read_bytes()
    elif req_body_inline:
        # Decode inline body (see decode_body() above)
        ...
```

---

## Logging Everything vs. Selective Logging

### Default behavior (selective)

```bash
pine windsurf
```

Only logs HTTPS traffic to curated hosts:
- `*.codeium.com`
- `*.windsurf.com`
- `api.anthropic.com`
- `api.openai.com`
- `api.claude.ai`
- etc. (see `--hosts` in the code)

### Discovery mode (everything)

```bash
pine windsurf --hosts '*'
```

Logs **every single HTTPS request** Windsurf makes. Useful for finding unknown endpoints.

### Blind-tunnel mode

Non-matching hosts are **blind-tunneled** (just piped through without decryption). You'll see the CONNECT line logged, but not the request/response bodies.

---

## Key Facts About Logging

1. **Append-only** — Once a byte lands on disk, it stays. No rotation, truncation, or cleanup.
2. **No size limits** — Sessions can grow arbitrarily large. Clean up manually with `rm -rf ~/.pine/sessions/<id>/`.
3. **Redaction** — By default, sensitive headers (`Authorization`, `Cookie`, `X-API-Key`) are hashed instead of logged verbatim. Use `--no-redact` to log them plaintext.
4. **Streaming** — SSE and WebSocket messages are logged separately in `stream/` with one JSON line per event.
5. **Timestamps** — Every request has `ts_start` and `ts_end` (RFC3339 format).
6. **Unique IDs** — Each request gets a unique `id` field for cross-referencing bodies and streams.

---

## Exploring Sessions from Python

### List all sessions

```python
from pathlib import Path

sessions_dir = Path.home() / ".pine" / "sessions"
for session_dir in sorted(sessions_dir.iterdir(), reverse=True):
    print(session_dir.name)
```

### Get the latest session

```python
sessions = [p for p in sessions_dir.iterdir() if p.is_dir()]
latest = max(sessions, key=lambda p: p.stat().st_mtime)
print(latest)
```

### Count requests by method

```python
import json
from collections import Counter

traffic_file = latest / "traffic.jsonl"
methods = Counter()

for line in traffic_file.read_text().splitlines():
    if line.strip():
        record = json.loads(line)
        methods[record["method"]] += 1

for method, count in methods.most_common():
    print(f"{method}: {count}")
```

### Find all POST requests to a specific host

```python
host_filter = "api.anthropic.com"

for line in traffic_file.read_text().splitlines():
    if not line.strip():
        continue
    record = json.loads(line)
    if record["method"] == "POST" and host_filter in record["url"]:
        print(f"{record['id']}: {record['url']}")
```

---

## Summary

| Aspect | Details |
|--------|---------|
| **What's logged** | Every HTTPS request/response matching your host filter |
| **Where** | `~/.pine/sessions/<SESSION_ID>/` |
| **Main file** | `traffic.jsonl` (one JSON line per request/response) |
| **Large bodies** | Spilled to `bodies/<id>.{req,res}.bin` (>64 KB) |
| **Streaming** | SSE/WebSocket logged separately in `stream/` |
| **Retention** | Forever (append-only, no cleanup) |
| **Python access** | Read `traffic.jsonl` line-by-line, decode bodies via `body_inline` or `body_path` |

