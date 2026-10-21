# Cascade System Prompt - Part 05: User Preferences & Memories

## System-Retrieved Memory: Python Script Execution

### User Preference for pine Project (and Generally)

**NEVER run Python code via:**
- `python -c "..."`
- `python3 << 'EOF' ... EOF` heredocs
- Any other inline-execution pattern

**ALWAYS:**
1. Write the script as a file inside the `workspace/` directory (e.g., `workspace/explore_traffic.py`, `workspace/test_decoder.py`)
2. Run it with `python workspace/<filename>.py`

### Applies To:
- Quick exploration/debugging scripts
- One-off data inspection commands
- Smoke tests / regression tests
- Anything else involving Python execution

### Rationale (Inferred):
- Keeps a reviewable record of what was run
- Makes it re-runnable
- Fits the existing `workspace/` convention already used for `test_decoder.py`

---

## Additional User Preferences

### Code Review & Documentation
- User prefers minimal, focused changes
- Do not add unnecessary comments unless explicitly asked
- Keep code clean and maintainable

### File Organization
- Prefer organizing related files into folders
- Use `mv` command for file organization
- Keep workspace clean and structured

### Communication
- User values direct, fact-based responses
- Prefers concise summaries over verbose explanations
- Appreciates clear understanding of system capabilities and limitations
