#!/usr/bin/env python3
"""
urls.py — list all URLs from a pine session, time-ascending.

Usage:
  python3 urls.py <session-id>
  python3 urls.py latest
  python3 urls.py latest --method POST
  python3 urls.py latest --host codeium
"""

import argparse, json, sys
from pathlib import Path

SESSIONS = Path.home() / ".pine" / "sessions"

DIM   = "\033[2;90m"
RESET = "\033[0m"
GREEN = "\033[92m"
YELLOW= "\033[93m"
CYAN  = "\033[96m"
RED   = "\033[91m"

def method_c(m):
    return {"GET":GREEN,"POST":YELLOW,"PUT":CYAN,"DELETE":RED,"OPTIONS":DIM,"HEAD":DIM}.get(m, "") + m + RESET

def status_c(s):
    if s is None: return DIM + "???" + RESET
    col = GREEN if s < 300 else (CYAN if s < 400 else (YELLOW if s < 500 else RED))
    return col + str(s) + RESET

def resolve(sid):
    if sid == "latest":
        dirs = sorted([d for d in SESSIONS.iterdir() if d.is_dir()], key=lambda d: d.name, reverse=True)
        return dirs[0] if dirs else None
    d = SESSIONS / sid
    if d.exists(): return d
    hits = [x for x in SESSIONS.iterdir() if x.is_dir() and x.name.startswith(sid.upper())]
    if len(hits) == 1: return hits[0]
    if hits: print(f"Ambiguous: {[h.name for h in hits]}"); sys.exit(1)
    print(f"Not found: {sid}"); sys.exit(1)

def main():
    ap = argparse.ArgumentParser(description="List URLs from a pine session")
    ap.add_argument("session", help="Session ID or 'latest'")
    ap.add_argument("--method", help="Filter by method")
    ap.add_argument("--host", help="Filter by hostname substring")
    args = ap.parse_args()

    d = resolve(args.session)
    traffic = d / "traffic.jsonl"
    if not traffic.exists():
        print("No traffic.jsonl"); sys.exit(1)

    # Load meta
    meta = {}
    mp = d / "meta.json"
    if mp.exists():
        try: meta = json.loads(mp.read_text())
        except: pass

    target = meta.get("target", "?")
    started = meta.get("started_at", "")[:19].replace("T", " ")
    print(f"\n{DIM}session{RESET}  {CYAN}{d.name}{RESET}")
    print(f"{DIM}target{RESET}   {target}   {DIM}started{RESET} {started}")
    print()

    entries = []
    with traffic.open() as f:
        for line in f:
            line = line.strip()
            if not line: continue
            try: e = json.loads(line)
            except: continue

            if args.method and e.get("method","").upper() != args.method.upper():
                continue
            if args.host:
                from urllib.parse import urlparse
                h = urlparse(e.get("url","")).hostname or ""
                if args.host.lower() not in h.lower(): continue

            entries.append(e)

    # Already in time-ascending order from the file
    for i, e in enumerate(entries, 1):
        ts  = e.get("ts_start", "")
        t   = ts[11:23] if len(ts) >= 23 else ts  # HH:MM:SS.mmm
        m   = e.get("method", "?")
        s   = e.get("response", {}).get("status")
        url = e.get("url", "?")
        print(f"  {DIM}{i:4d}{RESET}  {DIM}{t}{RESET}  {method_c(f'{m:<7}')}  {status_c(s)}  {url}")

    print(f"\n  {DIM}{len(entries)} requests{RESET}\n")

if __name__ == "__main__":
    main()
