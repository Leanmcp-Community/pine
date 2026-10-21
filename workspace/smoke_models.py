"""Smoke test for TrafficEntry.is_stream / is_upgrade / stream_marker."""
from __future__ import annotations

import os
import sys

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, REPO_ROOT)

from packages.tui.models import TrafficEntry  # noqa: E402


def case(label: str, **kwargs) -> None:
    e = TrafficEntry(
        id=kwargs.get("id", "x"),
        ts_start="",
        ts_end="",
        method=kwargs.get("method", "GET"),
        url=kwargs.get("url", "https://example.com/"),
        http_version="HTTP/1.1",
        upstream_addr="",
        request=kwargs.get("request", {"headers": []}),
        response=kwargs.get("response", {"status": 200, "headers": []}),
        error=None,
    )
    print(f"--- {label} ---")
    print(f"  is_stream={e.is_stream}  is_upgrade={e.is_upgrade}  marker={e.stream_marker!r}")


def main() -> int:
    case("plain 200 GET")
    case(
        "streamed SSE response",
        response={"status": 200, "headers": [], "is_stream": True},
    )
    case(
        "successful WS upgrade (101)",
        response={"status": 101, "headers": [["upgrade", "websocket"]]},
    )
    case(
        "WS attempt that got 404 (current Devin/pine state)",
        request={
            "headers": [
                ["sec-websocket-key", "x3JJHMbDL1EzLkh9GBhXDw=="],
                ["sec-websocket-version", "13"],
                ["sec-websocket-extensions", "permessage-deflate; client_max_window_bits"],
            ]
        },
        response={"status": 404, "headers": []},
    )
    case(
        "marker priority: streaming + upgrade → upgrade wins",
        request={"headers": [["sec-websocket-key", "abc"]]},
        response={"status": 101, "headers": [], "is_stream": True},
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
