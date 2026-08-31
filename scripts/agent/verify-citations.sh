#!/usr/bin/env bash
#
# Verify the CODE citations in a review, triage comment or PR body against the actual tree.
#
# Models hallucinate file paths and line numbers confidently, and they are poor at catching
# their own bad citations when asked to re-check. So this is a mechanical check rather than an
# instruction: it resolves every `path:line` and `path:N-M` reference and fails if the path is
# missing, the line is past end of file, a bare filename is ambiguous, or a quoted line does
# not appear at the line cited.
#
#   scripts/agent/verify-citations.sh <file>        # check against HEAD
#   scripts/agent/verify-citations.sh <file> <ref>  # check against a ref or SHA
#   ... | scripts/agent/verify-citations.sh -       # read the body from stdin
#
# Exit 0 = every citation resolved. Exit 1 = at least one did not; the output says which.
#
set -uo pipefail

allow_none=""
if [ "${1:-}" = "--allow-no-citations" ]; then allow_none="yes"; shift; fi

body="${1:-}"
ref="${2:-HEAD}"

if [ -z "$body" ]; then
  echo "usage: $0 [--allow-no-citations] <file-with-body|-> [git-ref]" >&2
  exit 2
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# Read the body before changing directory — the path may be relative to where we were called.
if [ "$body" = "-" ]; then cat > "$tmp/body"; else cat "$body" > "$tmp/body"; fi

# Citations are written repo-root-relative, and `git ls-files` is scoped to the current
# directory, so resolving a bare filename would silently depend on where this was invoked.
root="$(git rev-parse --show-toplevel 2>/dev/null)" || {
  echo "not inside a git work tree" >&2; exit 2; }
cd "$root" || exit 2

git ls-files > "$tmp/tracked" 2>/dev/null || : > "$tmp/tracked"

checked=0 failed=0

fail() { printf '  FAIL  %s\n' "$1"; failed=$((failed + 1)); }
pass() { printf '  ok    %s\n' "$1"; }

# Normalize for comparison: trim ends, collapse internal runs of whitespace.
norm() { printf '%s' "$1" | tr '\t' ' ' | sed -e 's/  */ /g' -e 's/^ //' -e 's/ $//'; }

echo "Verifying citations against ${ref}"

while IFS= read -r line; do
  # Every `something.ext:NNN` or `something.ext:NNN-MMM` inside inline code.
  cites="$(printf '%s' "$line" \
    | grep -oE -- '`[A-Za-z0-9_./-]+\.[A-Za-z0-9_]+:[0-9]+(-[0-9]+)?`' || true)"
  [ -n "$cites" ] || continue

  while IFS= read -r cite; do
    [ -n "$cite" ] || continue
    checked=$((checked + 1))

    bare="${cite//\`/}"
    path="${bare%:*}"
    lines="${bare##*:}"
    first="${lines%%-*}"
    last="${lines##*-}"

    # Resolve a bare filename to a tracked path, but only if it is unambiguous.
    if [ "${path#*/}" = "$path" ]; then
      matches="$(grep -E -- "(^|/)${path}$" "$tmp/tracked" || true)"
      count="$(printf '%s\n' "$matches" | grep -c . || true)"
      if [ "$count" = "1" ]; then
        path="$matches"
      elif [ "$count" = "0" ]; then
        fail "$cite — no tracked file named '${bare%:*}'"
        continue
      else
        fail "$cite — bare filename is ambiguous ($count matches); cite the full path"
        continue
      fi
    fi

    if ! git cat-file -e "${ref}:${path}" 2>/dev/null; then
      fail "$cite — '$path' does not exist at $ref"
      continue
    fi

    # grep -c '' counts lines; wc -l counts newlines, so a file whose last line has no
    # terminating newline would report one short and reject a correct citation of that line.
    total="$(git show "${ref}:${path}" 2>/dev/null | grep -c '' | tr -d ' ')"
    if [ "$last" -gt "$total" ] 2>/dev/null; then
      fail "$cite — line $last is past end of file ($path has $total lines at $ref)"
      continue
    fi

    actual="$(git show "${ref}:${path}" 2>/dev/null | sed -n "${first}p")"

    # If the same line quotes source text after the citation, it must appear at that line.
    rest="${line#*"$cite"}"
    quoted="$(printf '%s' "$rest" | grep -oE '`[^`]+`' | head -1 || true)"
    quoted="${quoted//\`/}"

    # A single-line citation is a verbatim claim about that line, so the quote must match.
    # A range citation points at a region; quoting a summary of it is legitimate prose, so a
    # range is only bounds-checked.
    if [ "$first" != "$last" ]; then
      pass "$cite — range within $path ($total lines)"
    elif [ -n "$quoted" ] && [ "${#quoted}" -ge 8 ]; then
      if [ -z "$(norm "$actual")" ]; then
        fail "$cite — quotes text, but line $first is blank at $ref"
      elif ! printf '%s' "$(norm "$actual")" | grep -qF -- "$(norm "$quoted")"; then
        fail "$cite — quoted text is not at line $first"
        printf '        cited:  %s\n' "$(norm "$quoted")"
        printf '        actual: %s\n' "$(norm "$actual")"
      else
        pass "$cite — $(norm "$actual")"
      fi
    else
      pass "$cite — $(norm "$actual")"
    fi
  done <<< "$cites"
done < "$tmp/body"

echo
if [ "$checked" -eq 0 ]; then
  if [ -n "$allow_none" ]; then
    echo "No CODE citations found; --allow-no-citations was passed, so this is fine."
    exit 0
  fi
  echo "No CODE citations found. If this body makes claims about how the code behaves,"
  echo "that is itself a protocol violation — see PROTOCOL.md."
  echo "For a body that legitimately cites nothing (a marker backfill, say), pass"
  echo "--allow-no-citations."
  exit 1
fi

echo "$checked citation(s) checked, $failed failed."
[ "$failed" -eq 0 ] || exit 1
