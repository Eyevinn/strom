#!/usr/bin/env bash
#
# Eligibility gates for the implementation stage (see FIX.md, Phase 2).
#
# Prints at most one eligible issue, the first gate that stopped every other candidate, and
# the issues where a decision exists but is not yet machine-readable — with the exact reply
# that would arm each one.
#
# The gates are boolean logic over data the GitHub API already holds. Keeping them here
# rather than in a prompt means they are deterministic, testable, and not competing for a
# model's attention with the rest of the protocol.
#
# IMPORTANT — agent comments are identified by their MARKER, never by their author. The
# tasks may run under the same GitHub account a human uses; keying on the login silently
# discards that human's decisions. A comment is the agent's iff it contains
# "<!-- strom-agent".
#
# Requires: gh (authenticated), bash, standard POSIX tools. No jq needed.
#
#   REPO=Eyevinn/strom MAINTAINERS="alice bob" scripts/agent/gates.sh
#
set -euo pipefail

REPO="${REPO:-Eyevinn/strom}"
# Logins whose /agent-fix reply may arm the implementation stage. A decision from anyone
# else — an issue reporter, for instance — is surfaced but never acted on.
MAINTAINERS="${MAINTAINERS:-srperens}"
LOG_ISSUE_TITLE="${LOG_ISSUE_TITLE:-Agent triage log}"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

is_maintainer() {
  # Substring match on a padded list rather than word-splitting $MAINTAINERS, so this behaves
  # the same if the file is ever sourced by a shell that does not split unquoted expansions.
  case " $MAINTAINERS " in *" $1 "*) return 0 ;; esac
  return 1
}

# --- Gate 5 input: open PRs, fetched once ------------------------------------------------
gh pr list --repo "$REPO" --state open --limit 200 \
  --json number,body,headRefName \
  -q '.[] | "\(.number)\t\(.headRefName)\t\(.body | gsub("[\n\t]"; " "))"' \
  > "$tmp/prs" 2>/dev/null || : > "$tmp/prs"

pr_implements() {
  # Prints the PR number that claims this issue, if any.
  # An agent fix PR is authoritative (its own marker or branch names the issue). A link
  # keyword in anyone's body is a weaker signal and can be a passing mention, so the caller
  # reports which PR matched rather than trusting it silently.
  local n="$1" hit
  hit="$(grep -E "(agent/fix-${n}-|strom-agent protocol=v3 kind=fix issue=${n}( |>))" "$tmp/prs" \
         | cut -f1 | head -1 || true)"
  [ -n "$hit" ] && { printf '%s' "$hit"; return 0; }
  hit="$(grep -iE "(fixes|refs|closes|resolves)[[:space:]]+#${n}([^0-9]|$)" "$tmp/prs" \
         | cut -f1 | head -1 || true)"
  [ -n "$hit" ] && { printf '%s' "$hit"; return 0; }
  return 1
}

marker_field() {
  printf '%s' "$1" | grep -oE -- "(^| )$2=[^ >]+" | head -1 | cut -d= -f2- || true
}

answer_flag() {
  # answer_flag "<reply line>" --accept-radius  ->  the value, or empty
  # The `--` matters: without it grep reads the flag name as one of its own options and every
  # override silently evaluates to empty, which quietly disables gates 3 and 4's escape hatch.
  printf '%s' "$1" | grep -oE -- "$2[[:space:]]+[A-Za-z0-9_,-]+" | head -1 | awk '{print $2}' || true
}

# --- Candidate issues -------------------------------------------------------------------
# sort=created&direction=asc so the loop is deterministic. Which issue actually gets built is
# decided after the loop, by the age of the decision that armed it.
gh api "repos/$REPO/issues?state=open&per_page=100&sort=created&direction=asc" --paginate \
  -q '.[] | select(.pull_request == null) | "\(.number)\t\(.title)"' \
  < /dev/null > "$tmp/issues"

declare -a armed_list=()     # answer_ts|issue|option|author|override — sorted after the loop
declare -a stopped=()
declare -a interpret=()

while IFS=$'\t' read -r num title; do
  [ -n "${num:-}" ] || continue
  [ "$title" = "$LOG_ISSUE_TITLE" ] && continue

  gh api "repos/$REPO/issues/$num/comments" --paginate -q '
    .[] | [ .created_at,
            .user.login,
            (if (.body | contains("<!-- strom-agent")) then "A" else "-" end),
            (if (.body | contains("protocol=v3 kind=triage")) then "T" else "-" end),
            ((.body | split("\n")
                    | map(select(contains("strom-agent protocol=v3 kind=triage")))
                    | last) // ""),
            (((.body | split("\n")
                     | map(select(test("^[[:space:]]*/agent-fix[[:space:]]")))
                     | first) // "")
             | gsub("[\t]"; " "))
          ] | @tsv' < /dev/null > "$tmp/c" 2>/dev/null || : > "$tmp/c"

  # Gate 1 — the newest agent-authored v3 triage comment wins. A marker backfill sorts last
  # by construction (it is posted after the triage it restates), which is what makes it the
  # marker this loop keeps.
  triage_ts="" marker=""
  while IFS=$'\t' read -r ts login is_agent has_t mline _fixline; do
    if [ "$is_agent" = "A" ] && [ "$has_t" = "T" ] && [ -n "$mline" ]; then
      triage_ts="$ts"; marker="$mline"
    elif [ "$is_agent" = "A" ] && [ "$has_t" = "T" ] && [ -z "$triage_ts" ]; then
      triage_ts="$ts"   # a v3 triage with no marker line at all
    fi
  done < "$tmp/c"

  if [ -z "$triage_ts" ]; then
    stopped+=("#$num|1|no protocol=v3 triage comment")
    continue
  fi

  verdict="$(marker_field "$marker" verdict)"
  radius="$(marker_field "$marker" radius)"
  excluded="$(marker_field "$marker" excluded)"
  ask="$(marker_field "$marker" ask)"
  work="$(marker_field "$marker" work)"

  # Whether a human decision exists is computed here, BEFORE the marker gates, so that a
  # decision is never hidden behind a mechanical problem with the marker. A maintainer who
  # has already answered must show up in the report even when nothing else about the issue
  # is machine-readable yet.
  maint_reply="" maint_token_line=""
  while IFS=$'\t' read -r ts login is_agent _has_t _mline fixline; do
    [ "$is_agent" = "A" ] && continue
    [[ "$ts" > "$triage_ts" ]] || continue
    is_maintainer "$login" || continue
    maint_reply="$login"
    [ -n "$fixline" ] && maint_token_line="$fixline"
  done < "$tmp/c"

  if [ -z "$marker" ] || [ -z "$verdict" ]; then
    if [ -n "$maint_reply" ]; then
      interpret+=("#$num|$maint_reply replied after the triage; marker predates the structured fields, so nothing can read it|re-triage or backfill the marker first (TRIAGE.md), then /agent-fix <option>")
    fi
    stopped+=("#$num|1|marker predates the structured fields; needs a backfill (TRIAGE.md)")
    continue
  fi

  # A NEEDS_INFO triage that has since been answered is the triage stage's problem, not this
  # one — but say so, because a standing v3 comment otherwise suppresses re-triage forever.
  if [ "$verdict" = "NEEDS_INFO" ]; then
    answered=""
    while IFS=$'\t' read -r ts _login is_agent _has_t _mline _fixline; do
      [ "$is_agent" = "A" ] && continue
      [[ "$ts" > "$triage_ts" ]] && answered="yes"
    done < "$tmp/c"
    if [ -n "$answered" ]; then
      stopped+=("#$num|1|NEEDS_INFO, but a human answered since: needs RE-TRIAGE, not a fix")
    else
      stopped+=("#$num|1|triage verdict is NEEDS_INFO")
    fi
    continue
  fi

  if [ "$verdict" != "CONFIRMED" ]; then
    stopped+=("#$num|1|triage verdict is $verdict, not CONFIRMED")
    continue
  fi

  # Gate 2 — a maintainer replied after the triage with the /agent-fix token.
  answer_line="" answer_by="" answer_ts="" maint_no_token="" nonmaint_token=""
  while IFS=$'\t' read -r ts login is_agent _has_t _mline fixline; do
    [ "$is_agent" = "A" ] && continue          # agent output, not a human decision
    [[ "$ts" > "$triage_ts" ]] || continue
    if [ -n "$fixline" ]; then
      if is_maintainer "$login"; then
        answer_line="$fixline"; answer_by="$login"; answer_ts="$ts"
      else
        nonmaint_token="$login"
      fi
    elif is_maintainer "$login"; then
      maint_no_token="$login"
    fi
  done < "$tmp/c"

  if [ -z "$answer_line" ]; then
    suggest="/agent-fix <option>"
    [ "$radius" != "LOCAL" ] && suggest="$suggest --accept-radius $radius"
    [ "$excluded" != "none" ] && suggest="$suggest --accept-excluded $excluded"
    if [ -n "$maint_no_token" ]; then
      interpret+=("#$num|$maint_no_token replied after the triage without the token|$suggest")
    elif [ -n "$nonmaint_token" ]; then
      interpret+=("#$num|/agent-fix came from $nonmaint_token, not a maintainer|$suggest")
    fi
    stopped+=("#$num|2|no maintainer /agent-fix reply (ask=${ask:-unset})")
    continue
  fi

  opt="$(printf '%s' "$answer_line" \
         | grep -oE '/agent-fix[[:space:]]+[A-Za-z0-9_-]+' | head -1 | awk '{print $2}' || true)"

  # A bare /agent-fix names no option, and a triage offers two by construction. Letting it
  # through would hand the design choice back to the model, which is the one thing this
  # stage must never do. Route it to the report instead.
  if [ -z "$opt" ]; then
    interpret+=("#$num|$answer_by used /agent-fix but named no option|/agent-fix <option-name> (see the Ask: on the issue)")
    stopped+=("#$num|2|/agent-fix present but no option named")
    continue
  fi

  # Gates 3 and 4 — read from the triage marker. A maintainer may override, but only by
  # naming the exact token, so the escape hatch is explicit and auditable.
  ovr_radius="$(answer_flag "$answer_line" --accept-radius)"
  ovr_excluded="$(answer_flag "$answer_line" --accept-excluded)"

  if [ "$radius" != "LOCAL" ] && [ "$ovr_radius" != "$radius" ]; then
    stopped+=("#$num|3|radius is ${radius:-unset}; needs --accept-radius ${radius:-?}")
    continue
  fi
  if [ "$excluded" != "none" ] && [ "$ovr_excluded" != "$excluded" ]; then
    stopped+=("#$num|4|excluded area(s) ${excluded:-unset}; needs --accept-excluded ${excluded:-?}")
    continue
  fi

  # Gate 5 — nobody is already implementing it.
  if claimed="$(pr_implements "$num")"; then
    stopped+=("#$num|5|PR #$claimed already claims it (check it really implements this)")
    continue
  fi

  ovr=""
  [ -n "$ovr_radius$ovr_excluded" ] && ovr="$answer_line"
  armed_list+=("$answer_ts|$num|$opt|$answer_by|$ovr|${work:-unset}")
done < "$tmp/issues"

# First decided, first built — an older maintainer decision must not wait behind a newer one.
eligible="" eligible_opt="" eligible_auth="" deferred=0
if [ "${#armed_list[@]}" -gt 0 ]; then
  while IFS='|' read -r a_ts a_num a_opt a_by a_ovr a_work; do
    if [ -z "$eligible" ]; then
      eligible="$a_num"; eligible_opt="$a_opt"; eligible_auth="$a_by"; eligible_work="$a_work"
      [ -n "$a_ovr" ] && eligible_auth="$a_by (override: $a_ovr)"
      eligible_since="$a_ts"
    else
      deferred=$((deferred + 1))
      stopped+=("#$a_num|-|eligible, deferred: this run implements #$eligible (decided $a_ts)")
    fi
  done < <(printf '%s\n' "${armed_list[@]}" | sort -t'|' -k1,1)
fi

# --- Report -----------------------------------------------------------------------------
if [ -n "$eligible" ]; then
  echo "ELIGIBLE: $eligible   option: $eligible_opt   authorised by: $eligible_auth"
  echo "          work=$eligible_work   decision dated $eligible_since (oldest armed issue wins)"
else
  echo "NONE"
fi

armed=0
if [ "${#stopped[@]}" -gt 0 ]; then
  echo
  echo "Stopped, by first failing gate:"
  printf '%s\n' "${stopped[@]}" | sort -t'|' -k2,2 \
    | while IFS='|' read -r ref gate why; do
        printf '  %-6s gate %-2s %s\n' "$ref" "$gate" "$why"
      done
fi

if [ "${#interpret[@]}" -gt 0 ]; then
  armed="${#interpret[@]}"
  echo
  echo "A HUMAN REPLIED, BUT NOTHING MACHINE-READABLE CAME OF IT ($armed) — not eligible."
  echo "Report these; do not decide on anyone's behalf. Each needs one reply to arm it:"
  printf '%s\n' "${interpret[@]}" | while IFS='|' read -r ref why suggest; do
    printf '  %-6s %s\n         reply: %s\n' "$ref" "$why" "$suggest"
  done
fi

echo
printf 'Queue depth: %s implementing, %s eligible and deferred, %s awaiting an answer.\n' \
  "$([ -n "$eligible" ] && echo 1 || echo 0)" "$deferred" "$armed"
