#!/usr/bin/env bash
# Browse local Pine sessions and upload the ones you pick.
#
#   bash workspace/pick_sync.sh            # newest 25, all agents
#   bash workspace/pick_sync.sh claude     # only Claude Code sessions
#   bash workspace/pick_sync.sh codex 40   # newest 40 Codex sessions
#
# Uploading a session = `pn checkpoint <name> --session <id>`. That sends only
# its compact conversation (KBs), never the raw capture in ~/.pine/sessions.
set -euo pipefail

AGENT="${1:-all}"
LIMIT="${2:-25}"
SESSIONS="$HOME/.pine/sessions"
PN="$(command -v pn || command -v pine || true)"

[[ -n "$PN" ]] || { echo "pn/pine not found on PATH. Install: curl -fsSL https://usepine.dev/install | bash"; exit 1; }
command -v jq >/dev/null || { echo "jq is required (brew install jq)"; exit 1; }
[[ -d "$SESSIONS" ]] || { echo "No sessions in $SESSIONS"; exit 1; }

# ── Collect: newest first, sessions with recorded traffic only ─────────────
IDS=() AGENTS=() LINES=()
for dir in $(ls -1d "$SESSIONS"/01* 2>/dev/null | sort -r); do
  [[ -f "$dir/meta.json" ]] || continue
  [[ -e "$dir/traffic.jsonl" || -d "$dir/stream" ]] || continue
  [[ -e "$dir/.pulled" ]] && continue # a teammate's session you downloaded
  target=$(jq -r '.target // "claude"' "$dir/meta.json")
  [[ "$AGENT" == "all" || "$AGENT" == "$target" ]] || continue
  started=$(jq -r '.started_at // ""' "$dir/meta.json" | cut -c1-16 | tr T ' ')
  folder=$(basename "$(jq -r '.cwd // "?"' "$dir/meta.json")")
  size=$(du -sh "$dir" 2>/dev/null | cut -f1)
  # Messages in the conversation that would be uploaded ("-" = none, or pn too old).
  msgs=$("$PN" transcript "$(basename "$dir")" 2>/dev/null | jq ".messages | length" 2>/dev/null || echo "-")
  printf "\r  reading sessions... %d" "$((${#IDS[@]} + 1))" >&2
  IDS+=("$(basename "$dir")")
  AGENTS+=("$target")
  LINES+=("$(printf "%-7s %-16s %6s %5s  %s" "$target" "$started" "$size" "${msgs:--}" "$folder")")
  (( ${#IDS[@]} >= LIMIT )) && break
done
printf "\r\033[K" >&2
((${#IDS[@]} > 0)) || { echo "No $AGENT sessions found."; exit 0; }

list() {
  echo
  printf "  %3s  %-7s %-16s %6s %5s  %s\n" "#" "agent" "started (UTC)" "raw" "msgs" "folder"
  for i in "${!IDS[@]}"; do printf '  %3d  %s\n' "$((i + 1))" "${LINES[$i]}"; done
  echo
  echo "  p <#>        preview what would be uploaded"
  echo "  u <# # ...>  upload those sessions   (e.g. u 1 4 7)"
  echo "  l            show the list again      q  quit"
}

preview() {
  local id="${IDS[$1]}" t
  if ! t=$("$PN" transcript "$id" 2>/dev/null); then
    echo "  Can't build a preview (needs the newest pn: run 'pn update'), or the session has no conversation."
    return
  fi
  echo
  echo "$t" | jq -r '"  \(.agent)  \(.model)   \(.turns) turns, \(.messages | length) messages, \(tostring | length / 1024 | floor) KB to upload"'
  echo "  ─────────────────────────────────────────────"
  echo "$t" | jq -r '.messages[] | select(.kind == "user" or .kind == "assistant")
    | "  \(if .kind == "user" then "you  " else "agent" end)  \(.text | gsub("\\s+"; " ") | .[0:140])"' | head -12
}

upload() {
  local n id name
  for n in "$@"; do
    [[ "$n" =~ ^[0-9]+$ ]] && (( n >= 1 && n <= ${#IDS[@]} )) || { echo "  skip '$n': not on the list"; continue; }
    id="${IDS[$((n - 1))]}"
    name="test-${AGENTS[$((n - 1))]}-$(echo "$id" | tail -c 7 | tr 'A-Z' 'a-z')"
    echo "  → #$n $id as \"$name\""
    "$PN" checkpoint "$name" --session "$id" || echo "  ✗ upload failed for $id"
  done
  echo
  echo "  Open them: https://app.usepine.dev/sessions   (or: pn open <id>)"
}

list
while read -r -p "> " cmd rest; do
  case "$cmd" in
    p) [[ "$rest" =~ ^[0-9]+$ ]] && (( rest >= 1 && rest <= ${#IDS[@]} )) && preview "$((rest - 1))" || echo "  usage: p <#>" ;;
    u)
      [[ -n "$rest" ]] || { echo "  usage: u <# # ...>"; continue; }
      read -r -p "  Upload ${rest// /, } (visible to everyone who can read their repo)? [y/N] " ok
      [[ "$ok" == [yY]* ]] && upload $rest ;;
    l) list ;;
    q | "") break ;;
    *) echo "  p <#>, u <#...>, l, q" ;;
  esac
done
