"""Survey a captured pine session for streaming / WebSocket signals.

Usage:
    python workspace/inspect_traffic.py [SESSION_ID]

If SESSION_ID is omitted, picks the most-recently-modified session under
~/.pine/sessions/.
"""
from __future__ import annotations

import json
import sys
from collections import Counter
from pathlib import Path


def latest_session(root: Path) -> Path:
    sessions = [p for p in root.iterdir() if p.is_dir()]
    if not sessions:
        raise SystemExit(f"no sessions under {root}")
    return max(sessions, key=lambda p: p.stat().st_mtime)


def header(headers: list, name: str) -> str:
    """Return the first matching header value (case-insensitive)."""
    for h in headers or []:
        if not h:
            continue
        try:
            k, v = h
        except (ValueError, TypeError):
            continue
        if str(k).lower() == name.lower():
            return str(v)
    return ""


def main() -> int:
    root = Path.home() / ".pine" / "sessions"
    if len(sys.argv) > 1:
        sess_dir = root / sys.argv[1]
    else:
        sess_dir = latest_session(root)
    print(f"Session: {sess_dir}\n")

    traffic = sess_dir / "traffic.jsonl"
    if not traffic.exists():
        raise SystemExit(f"no traffic.jsonl in {sess_dir}")

    total = 0
    streams = 0
    status_101 = 0
    upgrade_req = 0          # request had Upgrade header
    upgrade_ws = 0           # request had Upgrade: websocket
    sec_ws_key = 0           # request had Sec-WebSocket-Key header
    upgrade_resp_hdr = 0     # response had Upgrade header (server-sent upgrade)
    method_counts = Counter()
    suspicious_hosts = Counter()

    for line in traffic.read_text().splitlines():
        if not line.strip():
            continue
        try:
            d = json.loads(line)
        except json.JSONDecodeError:
            continue
        total += 1

        req = d.get("request", {})
        resp = d.get("response", {})
        req_h = req.get("headers", [])
        resp_h = resp.get("headers", [])

        if resp.get("is_stream"):
            streams += 1
        if resp.get("status") == 101:
            status_101 += 1
            host = d.get("url", "").split("://", 1)[-1].split("/", 1)[0]
            suspicious_hosts[host] += 1

        upgrade = header(req_h, "upgrade")
        if upgrade:
            upgrade_req += 1
            if "websocket" in upgrade.lower():
                upgrade_ws += 1
                host = d.get("url", "").split("://", 1)[-1].split("/", 1)[0]
                suspicious_hosts[host] += 1
        if header(req_h, "sec-websocket-key"):
            sec_ws_key += 1
        if header(resp_h, "upgrade"):
            upgrade_resp_hdr += 1

        method_counts[d.get("method", "?")] += 1

    print(f"Total entries:               {total}")
    print(f"is_stream=True (chunked):    {streams}")
    print(f"Response status 101:         {status_101}")
    print(f"Request had Upgrade hdr:     {upgrade_req}")
    print(f"  ...of which WebSocket:     {upgrade_ws}")
    print(f"Request had Sec-WebSocket-Key: {sec_ws_key}")
    print(f"Response had Upgrade hdr:    {upgrade_resp_hdr}")
    print()
    print("Methods:")
    for m, c in method_counts.most_common():
        print(f"  {m:<8} {c}")

    if suspicious_hosts:
        print("\nHosts that attempted upgrade / got 101:")
        for h, c in suspicious_hosts.most_common():
            print(f"  {c:>4}  {h}")

    # Sec-WebSocket-Key is a strong signal that the client tried to upgrade,
    # even when pine's hop-by-hop stripping removed Upgrade/Connection
    # before logging.
    print("\nAll entries with Sec-WebSocket-Key in request headers:")
    for line in traffic.read_text().splitlines():
        if not line.strip():
            continue
        try:
            d = json.loads(line)
        except json.JSONDecodeError:
            continue
        req = d.get("request", {})
        if not header(req.get("headers", []), "sec-websocket-key"):
            continue
        ver = header(req.get("headers", []), "sec-websocket-version")
        proto = header(req.get("headers", []), "sec-websocket-protocol")
        ext = header(req.get("headers", []), "sec-websocket-extensions")
        rid = d.get("id", "?")
        method = d.get("method", "?")
        status = d.get("response", {}).get("status")
        url = d.get("url", "")
        print(f"  id={rid}")
        print(f"    {method} {url}")
        print(f"    status={status}  ws-version={ver!r}  proto={proto!r}")
        print(f"    extensions={ext!r}")

    # Also inventory hosts to spot streaming-likely candidates.
    print("\nTop response content-types:")
    ct_counts: Counter[str] = Counter()
    for line in traffic.read_text().splitlines():
        if not line.strip():
            continue
        try:
            d = json.loads(line)
        except json.JSONDecodeError:
            continue
        ct_counts[
            header(d.get("response", {}).get("headers", []), "content-type")
            or "(none)"
        ] += 1
    for ct, c in ct_counts.most_common(12):
        print(f"  {c:>5}  {ct}")

    return 0


if __name__ == "__main__":
    sys.exit(main())
