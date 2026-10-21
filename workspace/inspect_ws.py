"""Read & decode WebSocket streams captured by pine.

Usage:
    python workspace/inspect_ws.py [SESSION_ID] [--req-id REQ_ID]

If SESSION_ID is omitted, picks the most recently modified session under
~/.pine/sessions/. If --req-id is given, only that exchange is dumped;
otherwise every WebSocket exchange in the session is summarised.

WHAT THIS SCRIPT DEMONSTRATES
-----------------------------
The Rust side does this for every WS frame (see src/ws.rs::proxy_ws_stream):
  * unmask the payload
  * write one JSON object per frame to
    ~/.pine/sessions/<sid>/stream/<req_id>.ws.jsonl
  * mark the parent traffic.jsonl record with
        response.is_websocket = true
        response.ws_path      = "stream/<req_id>.ws.jsonl"

So from Python all you need is:
  1. Scan traffic.jsonl for is_websocket==true.
  2. Open ws_path (relative to the session dir, like body_path).
  3. Parse each line as JSON. Fields: ts, dir, op, fin, len,
     payload | payload_b64, code, reason, rsv1/2/3.
  4. Reassemble fragmentation: keep concatenating cont frames per direction
     until you see fin=true.
  5. If rsv1 is set on the *first* frame of a message, the upstream
     negotiated permessage-deflate (RFC 7692). Inflate raw DEFLATE,
     remembering to append the 0x00 0x00 0xff 0xff trailer the spec
     strips before transmission.
"""
from __future__ import annotations

import argparse
import base64
import json
import sys
import zlib
from pathlib import Path


WS_TRAILER = b"\x00\x00\xff\xff"


def latest_session(root: Path) -> Path:
    sessions = [p for p in root.iterdir() if p.is_dir()]
    if not sessions:
        raise SystemExit(f"no sessions under {root}")
    return max(sessions, key=lambda p: p.stat().st_mtime)


def find_ws_entries(traffic_path: Path) -> list[dict]:
    """Return TrafficRecord dicts where response.is_websocket is true."""
    out: list[dict] = []
    for line in traffic_path.read_text().splitlines():
        if not line.strip():
            continue
        try:
            d = json.loads(line)
        except json.JSONDecodeError:
            continue
        if d.get("response", {}).get("is_websocket"):
            out.append(d)
    return out


def load_frames(ws_log: Path) -> list[dict]:
    """Load every frame record from a .ws.jsonl file."""
    frames: list[dict] = []
    for line in ws_log.read_text().splitlines():
        if not line.strip():
            continue
        try:
            frames.append(json.loads(line))
        except json.JSONDecodeError:
            pass
    return frames


def frame_payload_bytes(frame: dict) -> bytes:
    """Return the raw (unmasked, still-possibly-compressed) frame payload."""
    if "payload_b64" in frame and frame["payload_b64"]:
        return base64.b64decode(frame["payload_b64"])
    if "payload" in frame and frame["payload"] is not None:
        return frame["payload"].encode("utf-8")
    return b""


def reassemble_messages(frames: list[dict]) -> list[dict]:
    """Group fragmented frames into complete messages, per direction.

    A WS message is a sequence: <text|binary, fin=false>, <cont, fin=false>*,
    <cont, fin=true>. Control frames (close/ping/pong) are never fragmented
    and are emitted as their own message.

    Returned messages have:
        dir, op (text|binary|close|ping|pong), bytes, fin_ts, rsv1, frames
    """
    pending: dict[str, dict] = {}  # dir -> partial message accumulator
    messages: list[dict] = []

    for f in frames:
        d = f["dir"]
        op = f["op"]
        fin = f.get("fin", True)
        data = frame_payload_bytes(f)

        if op in ("close", "ping", "pong"):
            messages.append(
                {
                    "dir": d,
                    "op": op,
                    "bytes": data,
                    "fin_ts": f["ts"],
                    "rsv1": False,
                    "frames": 1,
                    "code": f.get("code"),
                    "reason": f.get("reason"),
                }
            )
            continue

        if op in ("text", "binary"):
            pending[d] = {
                "dir": d,
                "op": op,
                "bytes": bytearray(data),
                "fin_ts": f["ts"],
                "rsv1": bool(f.get("rsv1")),
                "frames": 1,
            }
        elif op == "cont":
            if d not in pending:
                # Out-of-order log — surface as best-effort
                pending[d] = {
                    "dir": d,
                    "op": "binary",
                    "bytes": bytearray(data),
                    "fin_ts": f["ts"],
                    "rsv1": bool(f.get("rsv1")),
                    "frames": 1,
                }
            else:
                pending[d]["bytes"].extend(data)
                pending[d]["fin_ts"] = f["ts"]
                pending[d]["frames"] += 1
        else:
            continue  # unknown opcode

        if fin and d in pending:
            msg = pending.pop(d)
            msg["bytes"] = bytes(msg["bytes"])
            messages.append(msg)

    # Flush anything dangling (truncated capture)
    for msg in pending.values():
        msg["bytes"] = bytes(msg["bytes"])
        msg["truncated"] = True
        messages.append(msg)

    return messages


class PermessageDeflateInflater:
    """Per-direction stateful inflater for permessage-deflate (RFC 7692).

    With `client_no_context_takeover` / `server_no_context_takeover` the
    decompressor is reset between messages; we conservatively *don't*
    assume that — most servers leave context-takeover on. If you see
    garbled output, set fresh=True.
    """

    def __init__(self, fresh: bool = False) -> None:
        self.fresh = fresh
        self._inflate = zlib.decompressobj(-zlib.MAX_WBITS)

    def decode(self, payload: bytes) -> bytes:
        if self.fresh:
            self._inflate = zlib.decompressobj(-zlib.MAX_WBITS)
        return self._inflate.decompress(payload + WS_TRAILER)


def maybe_decompress(messages: list[dict]) -> None:
    """In-place: if any message had rsv1 set, inflate raw DEFLATE per dir."""
    inflaters: dict[str, PermessageDeflateInflater] = {}
    for msg in messages:
        if msg["op"] in ("close", "ping", "pong"):
            continue
        if not msg.get("rsv1"):
            continue
        inf = inflaters.setdefault(msg["dir"], PermessageDeflateInflater())
        try:
            msg["bytes"] = inf.decode(msg["bytes"])
            msg["deflated"] = True
        except zlib.error as e:
            msg["deflate_error"] = str(e)


def render_message(msg: dict) -> str:
    arrow = "→" if msg["dir"] == "c2s" else "←"
    op = msg["op"]
    n = len(msg["bytes"])
    head = f"{arrow} {op:<6} {n:>6} B  ts={msg['fin_ts']}  frames={msg['frames']}"
    if msg.get("deflated"):
        head += "  [inflated]"
    if msg.get("truncated"):
        head += "  [truncated]"
    if msg.get("deflate_error"):
        head += f"  [deflate err: {msg['deflate_error']}]"

    if op == "close":
        head += f"  code={msg.get('code')!r} reason={msg.get('reason')!r}"
        return head

    body = msg["bytes"]
    if op == "text" or _looks_utf8(body):
        try:
            txt = body.decode("utf-8")
        except UnicodeDecodeError:
            return head + "\n  " + _hexdump(body)
        # Pretty-print JSON if applicable
        try:
            obj = json.loads(txt)
            txt = json.dumps(obj, indent=2, ensure_ascii=False)
        except json.JSONDecodeError:
            pass
        indented = "\n  ".join(txt.splitlines()[:40])
        n_lines = len(txt.splitlines())
        more = "" if n_lines <= 40 else f"\n  … ({n_lines} lines)"
        return head + "\n  " + indented + more

    return head + "\n  " + _hexdump(body)


def _looks_utf8(b: bytes) -> bool:
    try:
        b.decode("utf-8")
        return True
    except UnicodeDecodeError:
        return False


def _hexdump(b: bytes, limit: int = 256) -> str:
    out = []
    for i in range(0, min(len(b), limit), 16):
        chunk = b[i : i + 16]
        hex_part = " ".join(f"{x:02x}" for x in chunk)
        text_part = "".join(chr(x) if 32 <= x < 127 else "." for x in chunk)
        out.append(f"  {i:04x}  {hex_part:<48}  {text_part}")
    if len(b) > limit:
        out.append(f"  … ({len(b)} bytes total, showing first {limit})")
    return "\n".join(out)


def dump_exchange(record: dict, session_dir: Path) -> None:
    rid = record.get("id", "?")
    url = record.get("url", "?")
    status = record.get("response", {}).get("status")
    ws_path = record.get("response", {}).get("ws_path")
    print("=" * 78)
    print(f"id     : {rid}")
    print(f"url    : {url}")
    print(f"status : {status}  (101 = upgrade succeeded)")
    print(f"ws_path: {ws_path}")
    if not ws_path:
        return
    p = Path(ws_path)
    if not p.is_absolute():
        p = session_dir / p
    if not p.exists():
        print(f"  (file missing: {p})")
        return

    frames = load_frames(p)
    print(f"frames : {len(frames)}")

    # Detect permessage-deflate from the response handshake headers
    resp_headers = record.get("response", {}).get("headers", [])
    pmd = any(
        str(k).lower() == "sec-websocket-extensions"
        and "permessage-deflate" in str(v).lower()
        for kv in resp_headers
        for k, v in [kv if len(kv) == 2 else (None, None)]
        if k is not None
    )
    print(f"permessage-deflate negotiated: {pmd}")

    messages = reassemble_messages(frames)
    if pmd:
        maybe_decompress(messages)

    print(f"messages after reassembly: {len(messages)}\n")
    for msg in messages:
        print(render_message(msg))
        print()


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("session_id", nargs="?", default=None)
    ap.add_argument("--req-id", default=None, help="Only dump this exchange")
    args = ap.parse_args()

    root = Path.home() / ".pine" / "sessions"
    if args.session_id:
        sess_dir = root / args.session_id
    else:
        sess_dir = latest_session(root)
    print(f"Session: {sess_dir}\n")

    traffic = sess_dir / "traffic.jsonl"
    if not traffic.exists():
        raise SystemExit(f"no traffic.jsonl in {sess_dir}")

    ws_entries = find_ws_entries(traffic)
    if not ws_entries:
        print("No WebSocket exchanges (response.is_websocket==true) in this session.")
        print("Hint: pine only logs WS for hosts matched by --hosts. Run")
        print("      pine with the upgrading host in --hosts and retry.")
        return 0

    if args.req_id:
        ws_entries = [r for r in ws_entries if r.get("id") == args.req_id]
        if not ws_entries:
            raise SystemExit(f"no WS exchange with id={args.req_id}")

    for rec in ws_entries:
        dump_exchange(rec, sess_dir)

    return 0


if __name__ == "__main__":
    sys.exit(main())
