#!/bin/sh
# Shared check: flag Claude / Claude Code attribution in commit messages.
#   no-claude-attribution.sh check FILE   print offending lines, exit 1 on a hit
#   no-claude-attribution.sh strip FILE   remove offending lines in place, exit 1 on a hit
# '#' comment lines are ignored. `.claude/...` paths and CLAUDE.md are allowed.
# Used by .githooks/commit-msg and the commit-messages job in CI.

# The single pattern definition (ERE, applied to the lowercased line after
# allowed path-like tokens are stripped).
PATTERN='claude\.ai/code|claude-session|co-authored-by:.*(claude|anthropic)|generated with \[?claude|🤖.*generated|noreply@anthropic\.com|(^|[^a-z0-9_])claude([^a-z0-9_]|$)'
ALLOWED='\.claude(/[^[:space:]]*)?|claude\.md'

is_hit() {
  printf '%s\n' "$1" | tr 'A-Z' 'a-z' | sed -E "s#($ALLOWED)# #g" | grep -Eq -- "$PATTERN"
}

mode=$1
file=$2
[ -r "$file" ] || { echo "usage: $0 check|strip FILE" >&2; exit 2; }

tmp=$(mktemp) || exit 2
hits=0
while IFS= read -r line || [ -n "$line" ]; do
  case $line in
    '#'*) printf '%s\n' "$line" >>"$tmp"; continue ;;
  esac
  if is_hit "$line"; then
    hits=$((hits + 1))
    printf '  %s\n' "$line" >&2
  else
    printf '%s\n' "$line" >>"$tmp"
  fi
done <"$file"

if [ "$hits" -gt 0 ] && [ "$mode" = strip ]; then
  cat "$tmp" >"$file"
fi
rm -f "$tmp"
[ "$hits" -eq 0 ]
