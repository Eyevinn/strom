#!/usr/bin/env bash
#
# Report the state of the open board for the implementation stage (see FIX.md, Phase 2).
#
# This is a REPORT, not a gate. It answers questions with unambiguous answers — who replied
# after which triage, what the marker says, whether an open PR already claims the issue — and
# leaves every judgement to the reader.
#
# That division is deliberate, and it was learned the expensive way. An earlier version made
# all of it mechanical, including "which option did the maintainer choose" and "is the radius
# acceptable". It would have refused real work: on one issue the maintainer answered by
# rejecting both options the triage offered, naming a third, and explaining that the triage's
# radius classification applied only to the two it had proposed. No token vocabulary can
# express that, and reading prose is exactly what the model is good at.
#
# Three things stay hard, because they are unambiguous and being wrong is expensive:
#
#   1. No human reply after the triage at all -> nothing to implement. The open question is
#      the point of the triage stage, and a PR is an answer to it.
#   2. A reply only from someone who may not decide -> report it, never act on it.
#   3. An open PR already claims the issue -> do not duplicate it.
#
# Everything else is an observation for the reader to weigh, and FIX.md requires the reader to
# state its reading of each observation it acts past.
#
# Requires: gh (authenticated), bash, standard POSIX tools. No jq needed.
#
#   REPO=Eyevinn/strom MAINTAINERS="alice bob" scripts/agent/board.sh
#
# Set BOARD_LIB_ONLY=1 to source the helpers without running the report (used by the tests).
#
set -uo pipefail

REPO="${REPO:-Eyevinn/strom}"
# Logins whose reply may authorise implementation. Defaults to one login; if the deciding
# maintainer is not listed, every candidate is reported as unauthorised and the board looks
# empty. There is deliberately no AGENT_LOGIN: agent comments are identified by their marker,
# because these tasks may run under the same account a human uses.
MAINTAINERS="${MAINTAINERS:-srperens}"
LOG_ISSUE_TITLE="${LOG_ISSUE_TITLE:-Agent triage log}"

# The three line anchors, hoisted so the tests exercise the same strings the jq program does.
# Anchoring is load-bearing: GitHub's web "Quote reply" inserts the quoted markdown verbatim,
# marker included, so an unanchored match would read a human's answer as the agent's own AND
# treat the quoted marker as the newest triage, making every later reply look older than it.
MARKER_ANCHOR='^[[:space:]]*<!-- strom-agent'
TRIAGE_ANCHOR='^[[:space:]]*<!-- strom-agent protocol=v3 kind=triage'
FIXLINE_ANCHOR='^[[:space:]]*/agent-fix([[:space:]]|$)'

is_maintainer() {
  # Substring match on a padded list rather than word-splitting, so this behaves the same when
  # sourced by a shell that does not split unquoted expansions.
  case " $MAINTAINERS " in *" $1 "*) return 0 ;; esac
  return 1
}

marker_field() {
  # marker_field "<marker line>" radius  ->  the value, or empty.
  # The `--` stops grep reading a leading-dash argument as one of its own options.
  printf '%s' "$1" | grep -oE -- "(^| )$2=[^ >]+" | head -1 | cut -d= -f2- || true
}

named_option() {
  # The option token after /agent-fix, if the reply names one. Empty is a normal, common
  # answer shape — it means "read the prose", not "invalid".
  printf '%s' "$1" | grep -oE -- '/agent-fix[[:space:]]+[A-Za-z0-9_][A-Za-z0-9_-]*' \
    | head -1 | awk '{print $2}' || true
}

[ "${BOARD_LIB_ONLY:-}" = "1" ] && return 0

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# --- Open PRs, fetched once. No empty-list fallback: an empty list would make every issue
# --- look unclaimed, and the stage would open duplicates.
gh pr list --repo "$REPO" --state open --limit 200 \
  --json number,body,headRefName \
  -q '.[] | "\(.number)\t\(.headRefName)\t\(.body | gsub("[\n\t]"; " "))"' \
  < /dev/null > "$tmp/prs" \
  || { echo "cannot list open PRs; refusing to report a board that would look unclaimed" >&2; exit 3; }

claiming_pr() {
  # Each field is matched on its own. Grepping the whole record matched a PR whose *body*
  # merely quoted a branch name — this file's own PR did exactly that and was reported as
  # implementing an unrelated issue.
  local n="$1" prnum branch body
  while IFS=$'\t' read -r prnum branch body; do
    [ -n "${prnum:-}" ] || continue
    case "$branch" in
      agent/fix-"$n"-*|agent/feat-"$n"-*) printf '%s' "$prnum"; return 0 ;;
    esac
    case "$body" in
      *"strom-agent protocol=v3 kind=fix issue=$n "*|*"strom-agent protocol=v3 kind=fix issue=$n>"*)
        printf '%s' "$prnum"; return 0 ;;
    esac
    # A link keyword is a weaker signal than a branch or a marker: it can be a passing
    # mention, so the caller is told which PR matched rather than trusting it blindly.
    if printf '%s' "$body" \
       | grep -qiE -- "(fixes|refs|closes|resolves)[[:space:]]+#${n}([^0-9]|\$)"; then
      printf '%s' "$prnum"; return 0
    fi
  done < "$tmp/prs"
  return 1
}

gh api "repos/$REPO/issues?state=open&per_page=100&sort=created&direction=asc" --paginate \
  -q '.[] | select(.pull_request == null) | "\(.number)\t\(.title)"' \
  < /dev/null > "$tmp/issues" \
  || { echo "cannot list open issues" >&2; exit 3; }

declare -a candidates=()
declare -a claimed=()
declare -a unauthorised=()
declare -a waiting=()
declare -a other=()

while IFS=$'\t' read -r num title; do
  [ -n "${num:-}" ] || continue
  [ "$title" = "$LOG_ISSUE_TITLE" ] && continue

  # An agent comment is one with a marker at the START of a line. Anchoring matters: GitHub's
  # web "Quote reply" inserts the quoted markdown verbatim, marker included, so an unanchored
  # test would classify a human's answer as the agent's own — and would also treat the quoted
  # marker as the newest triage, making every later reply look older than the triage.
  gh api "repos/$REPO/issues/$num/comments" --paginate -q '
    .[] | [ .created_at,
            .user.login,
            (if (.body | split("\n") | map(select(test("'"$MARKER_ANCHOR"'"))) | length) > 0
             then "A" else "-" end),
            (if (.body | split("\n") | map(select(test("'"$TRIAGE_ANCHOR"'"))) | length) > 0
             then "T" else "-" end),
            (((.body | split("\n") | map(select(test("'"$TRIAGE_ANCHOR"'"))) | last) // "")
             | if length == 0 then "~" else . end),
            (((.body | split("\n") | map(select(test("'"$FIXLINE_ANCHOR"'"))) | first) // "")
             | gsub("[\t]"; " ") | if length == 0 then "~" else . end)
          ] | @tsv' < /dev/null > "$tmp/c" 2>/dev/null || : > "$tmp/c"


  # Fields arrive with "~" standing in for empty, because tab is IFS whitespace and bash
  # collapses runs of it: two adjacent empty fields would shift every later value one to the
  # left. That silently moved an /agent-fix line into the marker variable and reported a real
  # maintainer decision as "no token". Unmask at each read site.
  triage_ts="" marker=""
  while IFS=$'\t' read -r ts _login is_agent has_t mline _fix; do
    [ "$mline" = "~" ] && mline=""
    if [ "$is_agent" = "A" ] && [ "$has_t" = "T" ]; then
      triage_ts="$ts"
      [ -n "$mline" ] && marker="$mline"
    fi
  done < "$tmp/c"

  if [ -z "$triage_ts" ]; then
    other+=("#$num|no protocol=v3 triage comment yet")
    continue
  fi

  verdict="$(marker_field "$marker" verdict)"
  work="$(marker_field "$marker" work)"
  radius="$(marker_field "$marker" radius)"
  excluded="$(marker_field "$marker" excluded)"

  # Who replied since the triage, and how.
  m_ts="" m_by="" m_fix="" other_by=""
  while IFS=$'\t' read -r ts login is_agent _has_t _mline fixline; do
    [ "$fixline" = "~" ] && fixline=""
    [ "$is_agent" = "A" ] && continue
    [[ "$ts" > "$triage_ts" ]] || continue
    if is_maintainer "$login"; then
      m_ts="$ts"; m_by="$login"; [ -n "$fixline" ] && m_fix="$fixline"
    else
      other_by="$login"
    fi
  done < "$tmp/c"

  if [ -z "$m_by" ]; then
    if [ -n "$other_by" ]; then
      unauthorised+=("#$num|only $other_by replied since the triage, who may not decide")
    else
      waiting+=("#$num|verdict=${verdict:-unset}, no reply since the triage")
    fi
    continue
  fi

  # A maintainer replied. From here everything is an observation.
  notes=""
  opt="$(named_option "$m_fix")"
  if [ -z "$m_fix" ]; then
    notes="$notes; reply carries no /agent-fix token — read it and say whether it is a decision"
  elif [ -z "$opt" ]; then
    notes="$notes; /agent-fix names no option — the choice may not be one the triage offered"
  fi
  [ -z "$marker" ] && notes="$notes; triage marker predates the structured fields"
  [ -n "$marker" ] && [ -z "$radius" ] && notes="$notes; marker states no radius"
  [ -n "$radius" ] && [ "$radius" != "LOCAL" ] && \
    notes="$notes; marker says radius=$radius — re-assess it for the design actually chosen"
  [ -n "$excluded" ] && [ "$excluded" != "none" ] && \
    notes="$notes; marker says excluded=$excluded"
  [ "$verdict" != "CONFIRMED" ] && [ -n "$verdict" ] && \
    notes="$notes; triage verdict is $verdict"

  if cpr="$(claiming_pr "$num")"; then
    claimed+=("#$num|PR #$cpr already claims it|${notes#; }")
    continue
  fi
  candidates+=("$m_ts|#$num|$m_by|${opt:-(none named)}|${work:-unset}|${notes#; }")
done < "$tmp/issues"

# --- Report -----------------------------------------------------------------------------
echo "BOARD — $REPO"
echo

if [ "${#candidates[@]}" -gt 0 ]; then
  echo "CANDIDATES — a maintainer replied after the triage. Oldest decision first."
  echo "Pick at most one. You decide; state your reading of every note in the PR body."
  echo
  while IFS='|' read -r ts ref by opt work notes; do
    printf '  %-6s replied %s by %s\n' "$ref" "${ts%%T*}" "$by"
    printf '         option named: %s    marker work=%s\n' "$opt" "$work"
    [ -n "$notes" ] && printf '         notes: %s\n' "$(printf '%s' "$notes" | sed 's/; /\n                /g')"
    echo
  done < <(printf '%s\n' "${candidates[@]}" | sort -t'|' -k1,1)
else
  echo "CANDIDATES — none. No maintainer has replied after a triage on any unclaimed issue."
  echo
fi

if [ "${#claimed[@]}" -gt 0 ]; then
  echo "DECIDED BUT ALREADY CLAIMED — do not duplicate:"
  printf '%s\n' "${claimed[@]}" | while IFS='|' read -r ref why _n; do
    printf '  %-6s %s\n' "$ref" "$why"
  done
  echo
fi

if [ "${#unauthorised[@]}" -gt 0 ]; then
  echo "REPLIED, BUT NOT BY SOMEONE WHO MAY DECIDE — report, never act on:"
  printf '%s\n' "${unauthorised[@]}" | while IFS='|' read -r ref why; do
    printf '  %-6s %s\n' "$ref" "$why"
  done
  echo
fi

if [ "${#waiting[@]}" -gt 0 ]; then
  echo "AWAITING A FIRST REPLY (${#waiting[@]}) — nothing to implement:"
  printf '%s\n' "${waiting[@]}" | while IFS='|' read -r ref why; do
    printf '  %-6s %s\n' "$ref" "$why"
  done
  echo
fi

if [ "${#other[@]}" -gt 0 ]; then
  echo "NOT TRIAGED YET (${#other[@]}):"
  printf '%s\n' "${other[@]}" | while IFS='|' read -r ref why; do
    printf '  %-6s %s\n' "$ref" "$why"
  done
  echo
fi

printf 'Queue: %s candidate(s), %s decided-but-claimed, %s awaiting a first reply, %s untriaged.\n' \
  "${#candidates[@]}" "${#claimed[@]}" "${#waiting[@]}" "${#other[@]}"
