"""Screen 1 — Session list (newest first)."""

from __future__ import annotations

from pathlib import Path

from textual import on
from textual.app import ComposeResult
from textual.screen import Screen
from textual.widgets import DataTable, Footer, Header, Static

from ..models import SessionMeta

SESSIONS_DIR = Path.home() / ".pine" / "sessions"


class SessionListScreen(Screen):
    """Browse all pine sessions."""

    BINDINGS = [
        ("q", "quit", "Quit"),
        ("escape", "quit", "Quit"),
        ("r", "refresh", "Refresh"),
    ]

    def compose(self) -> ComposeResult:
        yield Header()
        yield Static(
            " 📡 pine sessions",
            id="session-header",
        )
        yield DataTable(id="session-table", cursor_type="row", zebra_stripes=True)
        yield Footer()

    def on_mount(self) -> None:
        self._load_sessions()

    def _load_sessions(self) -> None:
        table = self.query_one("#session-table", DataTable)
        table.clear(columns=True)
        table.add_columns("SESSION ID", "STARTED", "TARGET", "REQUESTS", "HOSTS")

        if not SESSIONS_DIR.exists():
            return

        sessions: list[SessionMeta] = []
        for d in SESSIONS_DIR.iterdir():
            if d.is_dir():
                sessions.append(SessionMeta.from_dir(d))

        # Newest first (ULIDs sort lexicographically by time)
        sessions.sort(key=lambda s: s.session_id, reverse=True)

        for s in sessions:
            table.add_row(
                s.session_id,
                s.started_short,
                s.target,
                str(s.entry_count),
                s.hosts_short,
                key=s.session_id,
            )

        # Store session map for lookup on select
        self._session_map = {s.session_id: s for s in sessions}

    @on(DataTable.RowSelected, "#session-table")
    def on_session_selected(self, event: DataTable.RowSelected) -> None:
        sid = event.row_key.value
        if sid and sid in self._session_map:
            session = self._session_map[sid]
            from .requests import RequestListScreen
            self.app.push_screen(RequestListScreen(session))

    def action_refresh(self) -> None:
        self._load_sessions()

    def action_quit(self) -> None:
        self.app.exit()
