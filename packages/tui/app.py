"""pine TUI — main application."""

from __future__ import annotations

import sys
from pathlib import Path

from textual.app import App

from .screens.sessions import SessionListScreen
from .screens.requests import RequestListScreen
from .models import SessionMeta

SESSIONS_DIR = Path.home() / ".pine" / "sessions"


class PineApp(App):
    """Terminal UI for browsing pine session logs."""

    TITLE = "pine"
    SUB_TITLE = "session browser"
    CSS_PATH = "theme.tcss"

    SCREENS = {
        "sessions": SessionListScreen,
    }

    BINDINGS = [
        ("q", "quit", "Quit"),
    ]

    def __init__(self, session_id: str | None = None, **kwargs):
        super().__init__(**kwargs)
        self._initial_session = session_id

    def on_mount(self) -> None:
        if self._initial_session:
            session_dir = self._resolve_session(self._initial_session)
            if session_dir:
                session = SessionMeta.from_dir(session_dir)
                # Push sessions screen first (so Esc from requests goes back here)
                self.push_screen(SessionListScreen())
                self.push_screen(RequestListScreen(session))
                return
        self.push_screen(SessionListScreen())

    def _resolve_session(self, sid: str) -> Path | None:
        if not SESSIONS_DIR.exists():
            return None

        if sid == "latest":
            dirs = sorted(
                [d for d in SESSIONS_DIR.iterdir() if d.is_dir()],
                key=lambda d: d.name,
                reverse=True,
            )
            return dirs[0] if dirs else None

        d = SESSIONS_DIR / sid
        if d.exists():
            return d

        # Prefix match
        hits = [
            x for x in SESSIONS_DIR.iterdir()
            if x.is_dir() and x.name.upper().startswith(sid.upper())
        ]
        if len(hits) == 1:
            return hits[0]
        return None
