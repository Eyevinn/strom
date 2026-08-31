#!/usr/bin/env bash
#
# Tests for board.sh and verify-citations.sh.
#
# These exist because every severe defect in these two scripts was invisible to careful
# reading — two separate code reviews read them and missed that the override parsing was dead
# code, that a marker with no radius passed the radius check, and that two adjacent empty
# fields shifted a maintainer's decision into the wrong variable. All three fell out of
# running them. A fail-open check is silent by construction, so it needs a fixture, not a
# careful reader.
#
# No network: board.sh is sourced with BOARD_LIB_ONLY=1, and the citation fixtures are built
# from files this repository actually tracks.
#
#   scripts/agent/test-agent-scripts.sh
#
set -uo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(git -C "$here" rev-parse --show-toplevel)"
cd "$root" || exit 2

pass=0 fail=0
ok()   { pass=$((pass+1)); printf '  ok    %s\n' "$1"; }
bad()  { fail=$((fail+1)); printf '  FAIL  %s\n' "$1"; [ $# -gt 1 ] && printf '        %s\n' "$2"; }
is()   { if [ "$2" = "$3" ]; then ok "$1"; else bad "$1" "expected [$3], got [$2]"; fi; }
exits() {
  local label="$1" want="$2"; shift 2
  "$@" >/dev/null 2>&1; local got=$?
  if [ "$got" = "$want" ]; then ok "$label"; else bad "$label" "expected exit $want, got $got"; fi
}

tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
V="$here/verify-citations.sh"

# ---------------------------------------------------------------------------------------
echo "board.sh — anchors"
# shellcheck disable=SC1090
BOARD_LIB_ONLY=1 . "$here/board.sh"

# A web "Quote reply" carries the agent's marker inside a quoted line. Treating that as the
# agent's own comment discards the human's decision and hijacks the triage timestamp.
quoted='> <!-- strom-agent protocol=v3 kind=triage issue=719 verdict=CONFIRMED -->'
plain='<!-- strom-agent protocol=v3 kind=triage issue=719 verdict=CONFIRMED -->'
printf '%s\n' "$quoted" | grep -qE -- "$MARKER_ANCHOR" \
  && bad "quoted marker is not agent output" "it matched MARKER_ANCHOR" \
  || ok "quoted marker is not agent output"
printf '%s\n' "$plain" | grep -qE -- "$MARKER_ANCHOR" \
  && ok "an unquoted marker is agent output" \
  || bad "an unquoted marker is agent output"
printf '%s\n' "$quoted" | grep -qE -- "$TRIAGE_ANCHOR" \
  && bad "quoted triage marker is not the newest triage" "it matched TRIAGE_ANCHOR" \
  || ok "quoted triage marker is not the newest triage"

# A bare token on its own line is a real answer shape and must be seen, so the reader is told
# "names no option" rather than "no token at all".
for line in '/agent-fix' '/agent-fix A' '  /agent-fix B'; do
  printf '%s\n' "$line" | grep -qE -- "$FIXLINE_ANCHOR" \
    && ok "arming line detected: '$line'" || bad "arming line detected: '$line'"
done
for line in '> `/agent-fix A` quoted' 'we should /agent-fix this later'; do
  printf '%s\n' "$line" | grep -qE -- "$FIXLINE_ANCHOR" \
    && bad "not an arming line: '$line'" || ok "not an arming line: '$line'"
done

echo "board.sh — marker fields"
M='<!-- strom-agent protocol=v3 kind=triage issue=719 verdict=CONFIRMED work=bug radius=LOCAL excluded=none ask=open -->'
is "verdict parsed"  "$(marker_field "$M" verdict)"  "CONFIRMED"
is "work parsed"     "$(marker_field "$M" work)"     "bug"
is "radius parsed"   "$(marker_field "$M" radius)"   "LOCAL"
is "excluded parsed" "$(marker_field "$M" excluded)" "none"
# An absent field must read as empty so the report says "states no radius" rather than
# treating silence as permission.
P='<!-- strom-agent protocol=v3 kind=triage issue=719 verdict=CONFIRMED ask=open -->'
is "absent radius is empty"   "$(marker_field "$P" radius)"   ""
is "absent excluded is empty" "$(marker_field "$P" excluded)" ""

echo "board.sh — option naming"
is "option named"            "$(named_option '/agent-fix A')" "A"
is "bare token names none"   "$(named_option '/agent-fix')"   ""
# A flag must not be read as the chosen option, or the stage arms itself with no decision.
is "flag is not an option"   "$(named_option '/agent-fix --accept-radius SHARED')" ""

echo "board.sh — maintainer allowlist"
MAINTAINERS="alice bob"
is_maintainer alice   && ok "listed login may decide"     || bad "listed login may decide"
is_maintainer bob     && ok "second listed login"          || bad "second listed login"
is_maintainer carol   && bad "unlisted login may not"      || ok "unlisted login may not"
is_maintainer ali     && bad "no substring match"          || ok "no substring match"

echo "board.sh — tab-collapse regression"
# Tab is IFS whitespace, so bash collapses a run of it. Two adjacent empty fields therefore
# shift every later value one to the left, which once moved an /agent-fix line into the
# marker variable and reported a real decision as "no token". Fields arrive masked with "~".
masked=$(printf '2026-08-31T09:42:59Z\tsrperens\t-\t-\t~\t/agent-fix\n')
# shellcheck disable=SC2034
while IFS=$'\t' read -r _ts _login _ag _ht mline fixline; do
  [ "$mline" = "~" ] && mline=""
  [ "$fixline" = "~" ] && fixline=""
  is "masked empty field keeps later values aligned" "$fixline" "/agent-fix"
  is "masked empty field unmasks to empty" "$mline" ""
done <<< "$masked"

# ---------------------------------------------------------------------------------------
echo "verify-citations.sh — citation resolution"
cat > "$tmp/good.md" <<'EOF'
A correct citation: `CLAUDE.md:1` — `## Project Overview`
EOF
exits "a correct citation passes" 0 "$V" "$tmp/good.md"

cat > "$tmp/wrongtext.md" <<'EOF'
Wrong quote: `CLAUDE.md:1` — `this text is definitely not there`
EOF
exits "quoted text not at the line fails" 1 "$V" "$tmp/wrongtext.md"

cat > "$tmp/eof.md" <<'EOF'
Past end: `rust-toolchain.toml:9999` — `nonsense`
EOF
exits "a line past end of file fails" 1 "$V" "$tmp/eof.md"

cat > "$tmp/missing.md" <<'EOF'
Missing: `backend/src/does_not_exist_at_all.rs:12` — `whatever`
EOF
exits "a missing path fails" 1 "$V" "$tmp/missing.md"

cat > "$tmp/ambiguous.md" <<'EOF'
Ambiguous: `mod.rs:3` — `something`
EOF
exits "an ambiguous bare filename fails" 1 "$V" "$tmp/ambiguous.md"

# A range points at a region; quoting a summary of it is legitimate prose, so a range is
# bounds-checked only and must not be text-compared.
cat > "$tmp/range.md" <<'EOF'
Region: `CLAUDE.md:1-8` — `a paraphrase of several lines`
EOF
exits "a range is bounds-checked, not text-compared" 0 "$V" "$tmp/range.md"

# Two citations on one line: the second must not be compared against the first one's line.
cat > "$tmp/twocites.md" <<'EOF'
The call at `CLAUDE.md:1` mirrors `rust-toolchain.toml:2-4` in shape.
EOF
exits "a second citation is not read as a quote" 0 "$V" "$tmp/twocites.md"

# A body whose last line has no terminating newline must still be read.
printf 'trailing `CLAUDE.md:1` — `## Project Overview`' > "$tmp/nonl.md"
out="$("$V" "$tmp/nonl.md" 2>&1)"
case "$out" in *"1 citation(s) checked"*) ok "last line without a newline is checked" ;;
  *) bad "last line without a newline is checked" "$(printf '%s' "$out" | tail -1)" ;; esac

echo "verify-citations.sh — no-citation and length handling"
echo 'A marker backfill cites nothing.' > "$tmp/nocite.md"
exits "a body with no citations fails by default" 1 "$V" "$tmp/nocite.md"
exits "--allow-no-citations permits it"          0 "$V" --allow-no-citations "$tmp/nocite.md"
# An unreadable body must abort rather than "verify" an empty file.
exits "an unreadable body aborts"                2 "$V" --allow-no-citations "$tmp/nope.md"
exits "an unreadable body aborts without the flag" 2 "$V" "$tmp/nope.md"

exits "over the length ceiling fails"  1 "$V" --max-chars 10 "$tmp/good.md"
exits "under the length ceiling passes" 0 "$V" --max-chars 4000 "$tmp/good.md"
exits "length applies with --allow-no-citations too" 1 \
  "$V" --allow-no-citations --max-chars 5 "$tmp/nocite.md"
exits "--max-chars without a number is a usage error" 2 "$V" --max-chars "$tmp/good.md"

echo "verify-citations.sh — independence from the working directory"
a="$("$V" "$tmp/good.md" 2>&1 | tail -1)"
b="$(cd backend/src && "$V" "$tmp/good.md" 2>&1 | tail -1)"
is "same result from a subdirectory" "$b" "$a"

# ---------------------------------------------------------------------------------------
echo
if [ "$fail" -eq 0 ]; then
  echo "$pass passed, 0 failed."
else
  echo "$pass passed, $fail FAILED."
fi
[ "$fail" -eq 0 ]
