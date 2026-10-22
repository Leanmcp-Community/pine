#!/usr/bin/env bash
# Watch an agent's own session file and show how fast each message lands in it.
#
#   bash workspace/watch_lag.sh codex    # newest file in ~/.codex/sessions
#   bash workspace/watch_lag.sh claude   # newest file in ~/.claude/projects
#   bash workspace/watch_lag.sh <file>   # a specific .jsonl
#
# How to test: start a session in Codex / Claude (terminal or desktop app),
# send one message so its file exists, run this, then keep chatting.
# For every new line it prints:
#   wall   when the line appeared in the file (this machine's clock)
#   lag    wall minus the timestamp the agent wrote inside the line
#   what   who/what the line is, plus the start of its text
# Ctrl-C to stop.
set -euo pipefail

# Newest file by modification time. awk reads the whole list (no `head`), so
# nothing in the pipe gets cut off early and trips pipefail.
newest() { find "$@" -exec stat -f '%m %N' {} + | awk '$1 > m { m = $1; f = substr($0, index($0, " ") + 1) } END { print f }'; }

case "${1:-codex}" in
  codex) FILE=$(newest "$HOME/.codex/sessions" -name 'rollout-*.jsonl') ;;
  claude) FILE=$(newest "$HOME/.claude/projects" -maxdepth 2 -name '*.jsonl') ;;
  *) FILE="$1" ;;
esac
[[ -f "$FILE" ]] || { echo "No session file found."; exit 1; }

echo "Watching $FILE"
echo "(lines already in the file are skipped; send a message now)"
printf '%-12s %8s  %s\n' "wall" "lag" "what"

# tail -F follows appends; perl timestamps each line the moment it arrives.
tail -n 0 -F "$FILE" 2>/dev/null | perl -MTime::HiRes=time -MJSON::PP -MTime::Local -ne '
  BEGIN { $| = 1; $json = JSON::PP->new }
  my $now = time;
  my $o = eval { $json->decode($_) } or next;
  # Codex: {"timestamp","type","payload"}; Claude: {"timestamp","type","message"}.
  my $ts = $o->{timestamp} // "";
  my $lag = "";
  if ($ts =~ /^(\d+)-(\d+)-(\d+)T(\d+):(\d+):(\d+)(\.\d+)?Z$/) {
    my $t = timegm($6, $5, $4, $3, $2 - 1, $1) + ($7 // 0);
    $lag = sprintf "%.0fms", ($now - $t) * 1000;
  }
  my $p = $o->{payload} // {};
  my $what = $o->{type} // "?";
  $what .= "/" . $p->{type} if ref $p eq "HASH" && $p->{type};
  $what .= "/" . ($p->{role} // $o->{message}{role} // "") if ($p->{role} // $o->{message}{role});
  my $text = "";
  for my $c (ref $p eq "HASH" && ref $p->{content} eq "ARRAY" ? @{$p->{content}} : ()) { $text .= ($c->{text} // "") . " " }
  my $mc = ref $o->{message} eq "HASH" ? $o->{message}{content} : undef;
  $text .= $mc if defined $mc && !ref $mc;
  for my $c (ref $mc eq "ARRAY" ? @$mc : ()) { $text .= ($c->{text} // "") . " " }
  $text =~ s/\s+/ /g;
  my @t = localtime $now;
  printf "%02d:%02d:%02d.%03d %8s  %-40s %s\n", @t[2,1,0], ($now - int $now) * 1000, $lag, substr($what, 0, 40), substr($text, 0, 70);
'
