#!/usr/bin/env python3
"""
Decode Pine/Codex websocket capture files.

These `*.ws.jsonl` files store websocket frames with:
- base64-encoded payload bytes
- optional per-message-deflate compression (`rsv1: true`)
- separate compression state per direction (`c2s` / `s2c`)

This script reconstructs the compression stream and prints decoded frames.
"""

from __future__ import annotations

import argparse
import base64
import json
import sys
import zlib
from pathlib import Path


WS_TRAILER = b"\x00\x00\xff\xff"


def decode_stream(path: Path, only_dir: str | None, raw: bool) -> int:
    inflaters: dict[str, zlib.decompressobj] = {}

    with path.open("r", encoding="utf-8") as f:
        for lineno, line in enumerate(f, start=1):
            line = line.strip()
            if not line:
                continue

            try:
                frame = json.loads(line)
            except json.JSONDecodeError as exc:
                print(f"[line {lineno}] invalid json: {exc}", file=sys.stderr)
                continue

            direction = frame.get("dir", "?")
            if only_dir and direction != only_dir:
                continue

            payload_b64 = frame.get("payload_b64")
            if payload_b64 is None:
                print(f"[line {lineno}] missing payload_b64", file=sys.stderr)
                continue

            try:
                payload = base64.b64decode(payload_b64)
            except Exception as exc:
                print(f"[line {lineno}] base64 decode failed: {exc}", file=sys.stderr)
                continue

            text = None
            error = None
            compressed = bool(frame.get("rsv1"))

            try:
                if compressed:
                    inflater = inflaters.setdefault(
                        direction, zlib.decompressobj(-zlib.MAX_WBITS)
                    )
                    out = inflater.decompress(payload)
                    if frame.get("fin"):
                        out += inflater.decompress(WS_TRAILER)
                    text = out.decode("utf-8", errors="replace")
                else:
                    text = payload.decode("utf-8", errors="replace")
            except Exception as exc:
                error = str(exc)

            meta = {
                "line": lineno,
                "ts": frame.get("ts"),
                "dir": direction,
                "op": frame.get("op"),
                "fin": frame.get("fin"),
                "len": frame.get("len"),
                "rsv1": frame.get("rsv1", False),
            }

            print(json.dumps(meta, ensure_ascii=False))

            if error:
                print(f"<decode error: {error}>")
            elif text is None:
                print("<no text>")
            elif raw:
                print(text)
            else:
                try:
                    parsed = json.loads(text)
                except json.JSONDecodeError:
                    print(text)
                else:
                    print(json.dumps(parsed, ensure_ascii=False, indent=2))

            print()

    return 0


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Decode Pine/Codex websocket capture files."
    )
    parser.add_argument("path", type=Path, help="Path to a *.ws.jsonl file")
    parser.add_argument(
        "--dir",
        choices=["c2s", "s2c"],
        help="Only decode one direction",
    )
    parser.add_argument(
        "--raw",
        action="store_true",
        help="Print decoded text as-is instead of pretty-printing JSON",
    )
    args = parser.parse_args()

    if not args.path.exists():
        print(f"file not found: {args.path}", file=sys.stderr)
        return 1

    return decode_stream(args.path, args.dir, args.raw)


if __name__ == "__main__":
    raise SystemExit(main())
