# Cascade System Prompt - Part 06: Workspace Context & Project Info

## Active Workspace

### Location
- **Path**: `/Users/ddod/PersonalCode/AIRTRAIN/ORIGINAL/CLAUDE_DOCTOR/pine`
- **CorpusName**: `rosaboyle/pine`
- **Git Root**: `/Users/ddod/PersonalCode/AIRTRAIN/ORIGINAL/CLAUDE_DOCTOR/pine`

### Operating System
- **OS**: macOS
- **Shell**: zsh

---

## Project Structure

### Root Level Files
- `Cargo.toml`, `Cargo.lock` - Rust project configuration
- `requirements.txt` - Python dependencies
- `tap.py` - Main Python entry point
- `urls.py` - URL configuration
- `build.rs` - Rust build script
- `README.md`, `WINDSURF.md`, `DEBUG.md` - Documentation
- `plan.md`, `websocket_plan.md` - Project planning documents

### Rust Source (`src/`)
- `banner.rs` - Banner/display logic
- `ca.rs` - Certificate authority handling
- `launcher.rs` - Application launcher
- `log.rs` - Logging functionality
- `main.rs` - Main Rust entry point
- Additional `.rs` files for core functionality

### Python TUI Package (`packages/tui/`)
- **Framework**: Textual (TUI framework)
- **Structure**:
  - `app.py` - Main TUI application
  - `decoder.py` - Message/data decoder
  - `screens/` - Screen components (detail, requests, sessions)
  - `widgets/` - Custom widgets
  - `__main__.py`, `__init__.py` - Package initialization

### Workspace Scripts (`workspace/`)
- `check_detail_import.py` - Import checking utility
- `check_scroll_api.py` - Scroll API inspection
- `inspect_traffic.py` - Traffic inspection tool
- `inspect_ws.py` - WebSocket inspection
- `test_decoder.py` - Decoder testing
- `cascade_prompt_docs/` - Cascade system prompt documentation (this folder)

---

## Technology Stack

### Backend
- **Language**: Rust
- **Build System**: Cargo

### Frontend/TUI
- **Framework**: Textual (Python TUI)
- **Language**: Python 3

### Communication
- **Protocol**: WebSocket
- **Purpose**: Real-time communication between Rust backend and Python TUI

### Development Tools
- **Version Control**: Git
- **IDE Integration**: Windsurf (with Cascade AI assistant)

---

## Key Project Characteristics

### Hybrid Architecture
- Rust backend for performance-critical operations
- Python TUI for user interface
- WebSocket bridge for communication

### Current Focus Areas
- Message decoding and inspection
- Traffic monitoring and analysis
- WebSocket communication handling
- TUI screen management and widgets

### Known Issues (from mypy linting)
- Missing type stubs for `textual` library
- Type annotation issues in decoder and tap modules
- Some dictionary access on potentially None values

---

## Development Workflow

### Python Script Convention
- All Python scripts for testing/debugging go in `workspace/`
- Run with `python workspace/<script_name>.py`
- Keeps workspace organized and scripts reviewable

### Code Changes
- Prefer minimal, focused edits
- Follow existing code style
- Add imports at top of files
- Test changes before committing

### Documentation
- Keep README and planning documents updated
- Use clear, concise commit messages
- Document architectural decisions in WINDSURF.md or DEBUG.md
