#!/usr/bin/env python3
"""
tap.py — browse pine session logs.

Usage:
  python3 tap.py                        # list all sessions (newest first)
  python3 tap.py <session-id>           # browse a session interactively
  python3 tap.py <session-id> --dump    # dump all entries to stdout
  python3 tap.py <session-id> -n 20     # show first 20 entries
  python3 tap.py <session-id> --host api.anthropic.com
  python3 tap.py <session-id> --method POST
  python3 tap.py <session-id> --ct json       # filter by content-type substring
  python3 tap.py <session-id> --index 5       # jump straight to entry #5
  python3 tap.py latest                       # open the most recent session
"""

import argparse
import base64
import gzip
import json
import os
import sys
import textwrap
from datetime import datetime, timezone
from pathlib import Path

# ── ANSI colours ────────────────────────────────────────────────────────────
RESET  = "\033[0m"
BOLD   = "\033[1m"
DIM    = "\033[2m"
RED    = "\033[91m"
GREEN  = "\033[92m"
YELLOW = "\033[93m"
BLUE   = "\033[94m"
MAGENTA= "\033[95m"
CYAN   = "\033[96m"
WHITE  = "\033[97m"
GREY   = "\033[90m"

def c(colour, text): return f"{colour}{text}{RESET}"
def bold(t):   return c(BOLD, t)
def dim(t):    return c(DIM + GREY, t)
def red(t):    return c(RED, t)
def green(t):  return c(GREEN, t)
def yellow(t): return c(YELLOW, t)
def blue(t):   return c(BLUE, t)
def magenta(t):return c(MAGENTA, t)
def cyan(t):   return c(CYAN, t)

SESSIONS_DIR = Path.home() / ".pine" / "sessions"

# ── Body decoding ────────────────────────────────────────────────────────────

def decode_body(body_inline: dict | None, body_path: str | None, content_type: str) -> tuple[str, str]:
    """
    Returns (decoded_text, label) where label is one of:
      "json", "text", "proto", "binary", "empty", "external"
    """
    if body_inline is None and not body_path:
        return "", "empty"

    if body_path:
        p = Path(body_path)
        if p.exists():
            raw = p.read_bytes()
        else:
            return f"(body file not found: {body_path})", "missing"
    else:
        enc = body_inline.get("encoding", "")
        data_str = body_inline.get("data", "")
        if not data_str:
            return "", "empty"
        if enc == "base64":
            # Fix padding
            missing = len(data_str) % 4
            if missing:
                data_str += "=" * (4 - missing)
            try:
                raw = base64.b64decode(data_str)
            except Exception as e:
                return f"(base64 decode error: {e})", "error"
        elif enc == "utf-8":
            raw = data_str.encode()
        else:
            raw = data_str.encode()

    # Decompress gzip
    if raw[:2] == b"\x1f\x8b":
        try:
            raw = gzip.decompress(raw)
        except Exception as e:
            return f"(gzip decompress error: {e})\nRaw (first 200 bytes): {raw[:200]!r}", "error"

    ct = content_type.lower()

    # JSON
    if "json" in ct or ct == "":
        try:
            obj = json.loads(raw)
            return json.dumps(obj, indent=2, ensure_ascii=False), "json"
        except Exception:
            pass

    # Plaintext
    if any(t in ct for t in ("text/", "application/connect+proto", "xml", "html", "svg", "csv")):
        try:
            return raw.decode("utf-8", errors="replace"), "text"
        except Exception:
            pass

    # Proto / binary — try to parse as JSON first (some gRPC-JSON)
    try:
        obj = json.loads(raw)
        return json.dumps(obj, indent=2, ensure_ascii=False), "json"
    except Exception:
        pass

    # Give up — hex-dump the first 512 bytes
    if "proto" in ct or "octet" in ct or "binary" in ct or "git" in ct:
        label = "proto" if "proto" in ct else "binary"
    else:
        label = "binary"

    hex_lines = []
    for i in range(0, min(len(raw), 512), 16):
        chunk = raw[i:i+16]
        hex_part  = " ".join(f"{b:02x}" for b in chunk)
        text_part = "".join(chr(b) if 32 <= b < 127 else "." for b in chunk)
        hex_lines.append(f"  {i:04x}  {hex_part:<48}  {text_part}")
    if len(raw) > 512:
        hex_lines.append(f"  ... ({len(raw)} bytes total, showing first 512)")
    return "\n".join(hex_lines), label

# ── Colour for HTTP method ───────────────────────────────────────────────────

def method_colour(method: str) -> str:
    return {
        "GET":     green(method),
        "POST":    yellow(method),
        "PUT":     blue(method),
        "DELETE":  red(method),
        "PATCH":   magenta(method),
        "HEAD":    dim(method),
        "OPTIONS": dim(method),
    }.get(method, cyan(method))

def status_colour(code: int | None) -> str:
    if code is None: return dim("???")
    s = str(code)
    if code < 300:   return green(s)
    if code < 400:   return cyan(s)
    if code < 500:   return yellow(s)
    return red(s)

def label_colour(label: str) -> str:
    return {
        "json":   green("JSON"),
        "text":   cyan("TEXT"),
        "proto":  magenta("PROTO"),
        "binary": magenta("BIN"),
        "empty":  dim("empty"),
        "error":  red("ERR"),
        "missing":red("MISSING"),
        "external": dim("ext"),
    }.get(label, dim(label))

# ── Header helpers ───────────────────────────────────────────────────────────

def get_header(headers: list, name: str) -> str:
    for h, v in headers:
        if h.lower() == name.lower():
            return v
    return ""

def fmt_headers(headers: list) -> str:
    lines = []
    for h, v in headers:
        lines.append(f"  {cyan(h)}: {v}")
    return "\n".join(lines)

# ── Duration helper ──────────────────────────────────────────────────────────

def duration_ms(ts_start: str, ts_end: str) -> str:
    try:
        fmt = "%Y-%m-%dT%H:%M:%S.%fZ"
        a = datetime.strptime(ts_start, fmt)
        b = datetime.strptime(ts_end,   fmt)
        ms = (b - a).total_seconds() * 1000
        return f"{ms:.0f}ms"
    except Exception:
        return "?"

# ── Terminal width ───────────────────────────────────────────────────────────

def term_width() -> int:
    try:
        return os.get_terminal_size().columns
    except Exception:
        return 100

def divider(char="─", colour=DIM+GREY) -> str:
    return c(colour, char * term_width())

def header_bar(text: str, colour=BOLD+CYAN) -> str:
    w = term_width()
    pad = max(0, w - len(text) - 4)
    return c(colour, f"┌── {text} " + "─" * pad + "┐")

# ── Format one entry ─────────────────────────────────────────────────────────

def format_entry(entry: dict, index: int, total: int, verbose: bool = True) -> str:
    out = []
    eid   = entry.get("id", "?")
    meth  = entry.get("method", "?")
    url   = entry.get("url", "?")
    ts    = entry.get("ts_start", "")
    te    = entry.get("ts_end", "")
    dur   = duration_ms(ts, te)
    err   = entry.get("error")

    req  = entry.get("request",  {})
    resp = entry.get("response", {})

    req_ct  = get_header(req.get("headers",  []), "content-type")
    resp_ct = get_header(resp.get("headers", []), "content-type")
    status  = resp.get("status")

    # ── Summary line ──────────────────────────────────────────────────────
    idx_str = dim(f"[{index+1}/{total}]")
    id_str  = dim(f"id={eid}")
    ts_str  = dim(ts.replace("T", " ").rstrip("Z") if ts else "")
    out.append(f"\n{divider('═', BOLD+BLUE)}")
    out.append(f"  {idx_str}  {method_colour(meth)}  {status_colour(status)}  {dur}  {ts_str}")
    out.append(f"  {bold(url)}")
    if id_str:
        out.append(f"  {id_str}")
    if err:
        out.append(f"  {red('ERROR:')} {err}")

    if not verbose:
        return "\n".join(out)

    # ── Request ───────────────────────────────────────────────────────────
    out.append(f"\n  {bold(cyan('REQUEST'))}")
    req_size = req.get("body_size", 0) or 0
    out.append(f"  {dim('headers:')}")
    out.append(fmt_headers(req.get("headers", [])))

    if req_size > 0:
        req_ct_disp = req_ct or "unknown"
        out.append(f"\n  {dim(f'body ({req_size} bytes, content-type: {req_ct_disp}):')}")
        body_txt, body_label = decode_body(
            req.get("body_inline"), req.get("body_path"), req_ct
        )
        out.append(f"  {dim('encoding:')} {label_colour(body_label)}")
        if body_txt:
            # Indent each line
            indented = "\n".join("    " + l for l in body_txt.splitlines())
            out.append(indented)
    else:
        out.append(f"  {dim('(no request body)')}")

    # ── Response ──────────────────────────────────────────────────────────
    out.append(f"\n  {bold(green('RESPONSE'))}  {status_colour(status)}")
    resp_size = resp.get("body_size", 0) or 0
    is_stream = resp.get("is_stream", False)
    stream_path = resp.get("stream_path")

    out.append(f"  {dim('headers:')}")
    out.append(fmt_headers(resp.get("headers", [])))

    if is_stream and stream_path:
        out.append(f"\n  {dim('streaming SSE log:')} {stream_path}")
        p = Path(stream_path)
        if p.exists():
            lines = p.read_text(errors="replace").splitlines()
            for l in lines[:40]:
                out.append("    " + l)
            if len(lines) > 40:
                out.append(f"    {dim(f'... {len(lines)-40} more lines')}")
    elif resp_size > 0:
        resp_ct_disp = resp_ct or "unknown"
        out.append(f"\n  {dim(f'body ({resp_size} bytes, content-type: {resp_ct_disp}):')} ")
        body_txt, body_label = decode_body(
            resp.get("body_inline"), resp.get("body_path"), resp_ct
        )
        out.append(f"  {dim('encoding:')} {label_colour(body_label)}")
        if body_txt:
            indented = "\n".join("    " + l for l in body_txt.splitlines())
            out.append(indented)
    else:
        out.append(f"  {dim('(no response body)')}")

    return "\n".join(out)

# ── One-line summary for browsing ────────────────────────────────────────────

def one_line(entry: dict, index: int) -> str:
    meth   = entry.get("method", "?")
    url    = entry.get("url", "?")
    status = entry.get("response", {}).get("status")
    ts     = entry.get("ts_start", "")
    dur    = duration_ms(ts, entry.get("ts_end",""))
    ts_s   = ts[11:19] if len(ts) >= 19 else ts   # HH:MM:SS

    w = term_width()
    url_max = max(30, w - 50)
    url_disp = url if len(url) <= url_max else "…" + url[-(url_max-1):]

    return (
        f"  {dim(f'{index+1:4d}')}"
        f"  {method_colour(f'{meth:<7}')}"
        f"  {status_colour(status)}"
        f"  {dim(ts_s)}"
        f"  {dim(f'{dur:>7}')}"
        f"  {url_disp}"
    )

# ── Session listing ──────────────────────────────────────────────────────────

def list_sessions():
    if not SESSIONS_DIR.exists():
        print(red(f"Sessions directory not found: {SESSIONS_DIR}"))
        sys.exit(1)

    sessions = []
    for d in SESSIONS_DIR.iterdir():
        if not d.is_dir(): continue
        meta_path = d / "meta.json"
        traffic_path = d / "traffic.jsonl"
        meta = {}
        if meta_path.exists():
            try: meta = json.loads(meta_path.read_text())
            except Exception: pass
        entry_count = 0
        if traffic_path.exists():
            try:
                entry_count = sum(1 for l in traffic_path.open() if l.strip())
            except Exception: pass
        sessions.append((d.name, meta, entry_count))

    # Sort by session ID descending (ULID = lexicographic time order)
    sessions.sort(key=lambda x: x[0], reverse=True)

    print(f"\n{bold('pine sessions')}  {dim(str(SESSIONS_DIR))}\n")
    print(f"  {'SESSION ID':<30}  {'STARTED':<22}  {'TARGET':<12}  {'ENTRIES'}")
    print(f"  {dim('─'*28)}  {dim('─'*20)}  {dim('─'*10)}  {dim('─'*7)}")

    for sid, meta, count in sessions:
        started = meta.get("started_at", "")[:19].replace("T", " ") if meta else ""
        target  = meta.get("target", "?") if meta else "?"
        host_filter = meta.get("host_filter", [])
        hf_str = ",".join(host_filter[:2]) if host_filter else ""
        if len(host_filter) > 2: hf_str += f"+{len(host_filter)-2}"
        print(
            f"  {cyan(sid)}"
            f"  {dim(started):<22}"
            f"  {yellow(target):<12}"
            f"  {count:<7}"
            f"  {dim(hf_str)}"
        )

    print(f"\n  {dim('Use: python3 tap.py <session-id>  or  python3 tap.py latest')}\n")

# ── Load entries with optional filters ──────────────────────────────────────

def load_entries(session_dir: Path, method: str | None, host: str | None, ct_filter: str | None) -> list:
    traffic = session_dir / "traffic.jsonl"
    if not traffic.exists():
        print(red(f"No traffic.jsonl in {session_dir}"))
        sys.exit(1)

    entries = []
    with traffic.open() as f:
        for line in f:
            line = line.strip()
            if not line: continue
            try:
                e = json.loads(line)
            except Exception:
                continue

            if method and e.get("method","").upper() != method.upper():
                continue

            url = e.get("url","")
            if host:
                try:
                    from urllib.parse import urlparse
                    h = urlparse(url).hostname or ""
                    if host.lower() not in h.lower():
                        continue
                except Exception:
                    pass

            if ct_filter:
                req_ct  = get_header(e.get("request",{}).get("headers",[]),  "content-type")
                resp_ct = get_header(e.get("response",{}).get("headers",[]), "content-type")
                if ct_filter.lower() not in (req_ct+resp_ct).lower():
                    continue

            entries.append(e)

    # Already in file order (ascending). Reverse for newest-first.
    entries.reverse()
    return entries

# ── Interactive browser ──────────────────────────────────────────────────────

def interactive_browse(entries: list, start_index: int = 0):
    if not entries:
        print(yellow("No entries to browse."))
        return

    idx = start_index
    total = len(entries)

    while True:
        if idx < 0: idx = 0
        if idx >= total: idx = total - 1

        # Clear screen
        print("\033[2J\033[H", end="")

        print(format_entry(entries[idx], idx, total, verbose=True))

        print(f"\n{divider()}")
        nav = (
            f"  {bold('[n]')}ext  "
            f"{bold('[p]')}rev  "
            f"{bold('[l]')}ist  "
            f"{bold('[g]')} goto #  "
            f"{bold('[f]')} filter url  "
            f"{bold('[q]')}uit"
        )
        print(nav)
        print(f"  {dim(f'Entry {idx+1} of {total}')}", end="  ", flush=True)

        try:
            ch = input().strip().lower()
        except (EOFError, KeyboardInterrupt):
            print()
            break

        if ch in ("q", "quit", "exit"):
            break
        elif ch in ("n", "", "next", "j"):
            idx += 1
        elif ch in ("p", "prev", "k"):
            idx -= 1
        elif ch in ("l", "list"):
            print("\033[2J\033[H", end="")
            for i, e in enumerate(entries):
                marker = cyan("►") if i == idx else " "
                print(f"{marker}{one_line(e, i)}")
            print(f"\n  {dim(f'{total} entries. Press Enter to return.')}", end=" ", flush=True)
            try: input()
            except (EOFError, KeyboardInterrupt): break
        elif ch in ("g", "goto"):
            print(f"  Go to entry # (1-{total}): ", end="", flush=True)
            try:
                n = int(input().strip()) - 1
                if 0 <= n < total: idx = n
            except (ValueError, EOFError):
                pass
        elif ch.startswith("f"):
            print("  Filter URL contains: ", end="", flush=True)
            try:
                filt = input().strip().lower()
                matched = [e for e in entries if filt in e.get("url","").lower()]
                if matched:
                    print(f"\n  {green(str(len(matched)))} matches. Press Enter to browse them, or Ctrl-C to cancel.", flush=True)
                    try:
                        input()
                        entries = matched
                        total = len(entries)
                        idx = 0
                    except (EOFError, KeyboardInterrupt):
                        pass
                else:
                    print(red("  No matches."), flush=True)
                    try: input()
                    except (EOFError, KeyboardInterrupt): pass
            except (EOFError, KeyboardInterrupt):
                pass
        elif ch.isdigit():
            n = int(ch) - 1
            if 0 <= n < total: idx = n

# ── Dump all ─────────────────────────────────────────────────────────────────

def dump_all(entries: list, n: int | None):
    total = len(entries)
    subset = entries[:n] if n else entries
    for i, e in enumerate(subset):
        print(format_entry(e, i, total, verbose=True))
    print(f"\n{dim(f'  {len(subset)} of {total} entries')}\n")

# ── Main ─────────────────────────────────────────────────────────────────────

def resolve_session(sid: str) -> Path:
    if sid == "latest":
        last = SESSIONS_DIR / "last-session"
        if last.exists():
            lines = [l.strip() for l in last.read_text().splitlines() if l.strip()]
            if lines:
                sid = lines[0]
        else:
            # Fallback: pick newest directory
            dirs = sorted(
                [d for d in SESSIONS_DIR.iterdir() if d.is_dir()],
                key=lambda d: d.name,
                reverse=True
            )
            if dirs:
                return dirs[0]
            print(red("No sessions found."))
            sys.exit(1)

    d = SESSIONS_DIR / sid
    if not d.exists():
        # Try prefix match
        matches = [x for x in SESSIONS_DIR.iterdir() if x.is_dir() and x.name.startswith(sid.upper())]
        if len(matches) == 1:
            return matches[0]
        elif len(matches) > 1:
            print(yellow(f"Ambiguous prefix '{sid}'. Matches:"))
            for m in matches: print(f"  {m.name}")
            sys.exit(1)
        else:
            print(red(f"Session '{sid}' not found in {SESSIONS_DIR}"))
            sys.exit(1)
    return d


def main():
    parser = argparse.ArgumentParser(
        description="Browse pine session logs.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=textwrap.dedent("""
          Examples:
            python3 tap.py                         # list all sessions
            python3 tap.py latest                  # browse newest session
            python3 tap.py 01KRB9T8               # prefix match OK
            python3 tap.py 01KRB9T8 --dump        # dump all to stdout
            python3 tap.py 01KRB9T8 -n 10         # show 10 most recent
            python3 tap.py 01KRB9T8 --method POST
            python3 tap.py 01KRB9T8 --host anthropic.com
            python3 tap.py 01KRB9T8 --ct json
            python3 tap.py 01KRB9T8 --index 5     # jump to entry #5
        """)
    )
    parser.add_argument("session", nargs="?", help="Session ID (or 'latest')")
    parser.add_argument("--dump",   action="store_true", help="Dump all entries non-interactively")
    parser.add_argument("-n",       type=int, metavar="N", help="Show only first N entries")
    parser.add_argument("--method", help="Filter by HTTP method (GET, POST, …)")
    parser.add_argument("--host",   help="Filter by hostname substring")
    parser.add_argument("--ct",     help="Filter by content-type substring")
    parser.add_argument("--index",  type=int, default=0, metavar="N", help="Start at entry N (1-based)")
    args = parser.parse_args()

    if not args.session:
        list_sessions()
        return

    session_dir = resolve_session(args.session)

    # Print session metadata
    meta_path = session_dir / "meta.json"
    if meta_path.exists():
        try:
            meta = json.loads(meta_path.read_text())
            started = meta.get("started_at","")[:19].replace("T"," ")
            target  = meta.get("target","?")
            ver     = meta.get("pine_version","?")
            pid     = meta.get("claude_pid","?")
            hf      = ",".join(meta.get("host_filter",[]))
            print(f"\n{bold('Session')} {cyan(session_dir.name)}")
            print(f"  {dim('target:')}   {yellow(target)}")
            print(f"  {dim('started:')}  {started}")
            print(f"  {dim('hosts:')}    {hf}")
            print(f"  {dim('pid:')}      {pid}   {dim('pine:')} {ver}")
            print(f"  {dim('logs:')}     {session_dir}")
        except Exception:
            pass

    entries = load_entries(session_dir, args.method, args.host, args.ct)

    print(f"\n  {bold(str(len(entries)))} entries {dim('(newest first)')}\n")

    if not entries:
        print(yellow("  No entries match your filters."))
        return

    if args.dump or args.n:
        dump_all(entries, args.n)
    else:
        start = max(0, args.index - 1) if args.index else 0
        interactive_browse(entries, start_index=start)


if __name__ == "__main__":
    main()
