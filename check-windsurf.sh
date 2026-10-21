#!/usr/bin/env bash
# Diagnose pine + Windsurf setup. Prints PASS/FAIL for each check.
set -u

pass() { printf "  \033[32m✓\033[0m %s\n" "$1"; }
fail() { printf "  \033[31m✗\033[0m %s\n" "$1"; FAILED=1; }
info() { printf "  · %s\n" "$1"; }

FAILED=0

echo "1. pine binary"
if command -v pine >/dev/null; then
  pass "found at $(command -v pine) ($(pine --version 2>/dev/null))"
else
  fail "pine not on PATH — run ./install.sh"
fi

echo
echo "2. Windsurf.app"
WSF=/Applications/Windsurf.app/Contents/MacOS/Windsurf
if [ -x "$WSF" ]; then
  pass "Windsurf binary at $WSF"
else
  fail "$WSF not executable"
fi

echo
echo "3. Root CA on disk"
CA=$HOME/.pine/ca/root.crt
if [ -f "$CA" ]; then
  pass "CA at $CA"
  openssl x509 -in "$CA" -noout -subject -dates 2>/dev/null | sed 's/^/      /'
else
  fail "missing $CA — run: pine ca trust"
fi

echo
echo "4. Root CA in login keychain"
if security find-certificate -c "pine local root" login.keychain >/dev/null 2>&1; then
  pass "found in login keychain"
else
  fail "not in login keychain — run: pine ca trust"
fi

echo
echo "5. Trust policy applied (the thing Chromium needs)"
TRUST=$(security dump-trust-settings 2>&1 | grep -c "pine")
if [ "$TRUST" -gt 0 ]; then
  pass "user-domain trust policy present"
else
  fail "no trust policy — cert is in keychain but not trusted for SSL"
  info "fix: pine ca untrust && pine ca trust (allow the dialog)"
fi

echo
echo "6. Windsurf is currently quit"
if pgrep -fl -i windsurf | grep -v pine >/dev/null 2>&1; then
  fail "Windsurf is running — ⌘Q it first (Electron singleton lock will swallow our launch)"
  pgrep -fl -i windsurf | sed 's/^/      /'
else
  pass "no Windsurf processes alive"
fi

echo
if [ "$FAILED" -eq 0 ]; then
  printf "\033[32mAll checks passed.\033[0m  Launch with:\n"
  echo "  rm -rf /tmp/wsf-tap && mkdir -p /tmp/wsf-tap"
  echo "  pine windsurf --user-data-dir /tmp/wsf-tap"
else
  printf "\033[31mFix the ✗ items above, then re-run this script.\033[0m\n"
  exit 1
fi
