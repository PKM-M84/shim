#!/bin/bash
# Smoke test for install.sh's Claude-settings merge (merge_claude_setting).
# Exercises the real code via `install.sh --merge-claude-config <file>` across
# fresh / empty / existing / idempotent cases and the jq + python3 engines.
# No root, no install — only writes to throwaway temp files.
#
#   bash tests/install_test.sh
#
# Exits non-zero if any assertion fails.

set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
INSTALL="$HERE/../install.sh"
PASS=0; FAIL=0

# read a dotted path (e.g. env.USE_BUILTIN_RIPGREP) out of a JSON file
read_json() {
  if command -v python3 >/dev/null 2>&1; then
    python3 - "$1" "$2" <<'PY'
import json, sys
cur = json.load(open(sys.argv[1]))
for k in sys.argv[2].split('.'):
    cur = cur[k]
print(cur)
PY
  else
    jq -r ".$2" "$1"
  fi
}
valid_json() {
  if command -v python3 >/dev/null 2>&1; then
    python3 -c 'import json,sys; json.load(open(sys.argv[1]))' "$1" 2>/dev/null
  else
    jq empty "$1" 2>/dev/null
  fi
}
# merge USE_BUILTIN_RIPGREP into $1, optional engine in $2
do_merge() {
  local file="$1" engine="${2:-auto}"
  SMARTRG_JSON_ENGINE="$engine" bash "$INSTALL" --merge-claude-config "$file" >/dev/null 2>&1
}
check() { # description, expected, actual
  if [ "$2" = "$3" ]; then PASS=$((PASS+1)); printf "  ✅ %s\n" "$1"
  else FAIL=$((FAIL+1)); printf "  ❌ %s  (expected '%s', got '%s')\n" "$1" "$2" "$3"; fi
}
check_validjson() { if valid_json "$2"; then PASS=$((PASS+1)); printf "  ✅ %s\n" "$1"; else FAIL=$((FAIL+1)); printf "  ❌ %s  (invalid JSON)\n" "$1"; fi; }

TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT

echo "Testing install.sh settings merge ($INSTALL)"

# 1. fresh file (does not exist)
f="$TMP/fresh/settings.json"
do_merge "$f"
check_validjson "fresh: valid JSON" "$f"
check "fresh: var set" "0" "$(read_json "$f" env.USE_BUILTIN_RIPGREP)"

# 2. empty file
f="$TMP/empty.json"; : > "$f"
do_merge "$f"
check "empty: var set" "0" "$(read_json "$f" env.USE_BUILTIN_RIPGREP)"

# 3. existing settings with other keys must be preserved
f="$TMP/existing.json"; printf '{"foo":1,"env":{"BAR":"baz"}}' > "$f"
do_merge "$f"
check_validjson "existing: valid JSON" "$f"
check "existing: var set"        "0"   "$(read_json "$f" env.USE_BUILTIN_RIPGREP)"
check "existing: sibling env key kept" "baz" "$(read_json "$f" env.BAR)"
check "existing: top-level key kept"   "1"   "$(read_json "$f" foo)"

# 4. idempotent — running again changes nothing meaningful
do_merge "$f"
check "idempotent: var still set"      "0"   "$(read_json "$f" env.USE_BUILTIN_RIPGREP)"
check "idempotent: sibling still kept" "baz" "$(read_json "$f" env.BAR)"

# 5. existing settings with NO env key
f="$TMP/noenv.json"; printf '{"foo":1}' > "$f"
do_merge "$f"
check "no-env: var set"          "0" "$(read_json "$f" env.USE_BUILTIN_RIPGREP)"
check "no-env: top-level kept"   "1" "$(read_json "$f" foo)"

# 6. env present but NOT an object (robustness)
f="$TMP/badenv.json"; printf '{"env":"oops"}' > "$f"
do_merge "$f"
check_validjson "bad-env: valid JSON" "$f"
check "bad-env: var set" "0" "$(read_json "$f" env.USE_BUILTIN_RIPGREP)"

# 7. forced jq engine
if command -v jq >/dev/null 2>&1; then
  f="$TMP/jq.json"; printf '{"env":{"BAR":"baz"}}' > "$f"
  do_merge "$f" jq
  check "engine=jq: var set"        "0"   "$(read_json "$f" env.USE_BUILTIN_RIPGREP)"
  check "engine=jq: sibling kept"   "baz" "$(read_json "$f" env.BAR)"
else
  echo "  ⏭️  engine=jq skipped (jq not installed)"
fi

# 8. forced python3 engine
if command -v python3 >/dev/null 2>&1; then
  f="$TMP/py.json"; printf '{"env":{"BAR":"baz"}}' > "$f"
  do_merge "$f" python3
  check "engine=python3: var set"      "0"   "$(read_json "$f" env.USE_BUILTIN_RIPGREP)"
  check "engine=python3: sibling kept" "baz" "$(read_json "$f" env.BAR)"
else
  echo "  ⏭️  engine=python3 skipped (python3 not installed)"
fi

# 9. --help and --check must succeed with no root and make no changes
bash "$INSTALL" --help  >/dev/null 2>&1; check "--help exits 0"  "0" "$?"
bash "$INSTALL" --check >/dev/null 2>&1; check "--check exits 0" "0" "$?"

echo ""
echo "Result: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
