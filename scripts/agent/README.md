# Agent review protocol

Strom's open PRs and issues are worked by two scheduled agents: one reviews PRs and triages
issues, the other turns an approved design into a draft PR. This directory is the protocol
they follow.

**It lives in the repo on purpose.** The protocol used to be embedded in the task
definitions, where it could not be diffed, reviewed or corrected by anyone but the person who
owned the schedule. Here it is version-controlled, and a PR against it is a PR against how
the bots behave.

## Files

| File | Read when |
|---|---|
| `PROTOCOL.md` | Every run, first. Evidence rules, citation form, controlled vocabulary, markers. |
| `REVIEW.md` | Reviewing a pull request. |
| `TRIAGE.md` | Triaging an issue. |
| `FIX.md` | Turning an approved design into a draft PR. |
| `SUMMARY.md` | Writing the run summary. Every run, last. |
| `board.sh` | Reporting the state of the open board before implementing anything. |
| `verify-citations.sh` | Before posting anything that cites code, or that has a length ceiling. |
| `test-agent-scripts.sh` | After changing either script. |

Read `PROTOCOL.md` plus the one file for the item in front of you — not all of them. The
protocol is deliberately split so that a run only carries the rules it is about to apply.

## What is deliberately *not* here

Nothing about where the agents run, what credentials they hold, how they are scheduled, or
where they report. That is deployment configuration and lives with the task definitions,
outside this repo. These files describe only what good output looks like, so they stay
useful if the runtime changes and safe to read in public.

## Design notes, so the next change does not undo them

- **The agent is identified by its marker, never by its GitHub account.** These tasks may run
  under the same account a human uses, and keying on the login makes the tooling discard that
  human's own decisions while reporting an empty queue. Both scripts and `PROTOCOL.md` say
  this; do not "simplify" it back to an author check.
- **`board.sh` reports; it does not decide.** It answers only what has an unambiguous answer —
  who replied after which triage, what the marker says, whether an open PR already claims the
  issue — and hands every judgement to the reader. An earlier version made all of it
  mechanical, including "which option was chosen" and "is the radius acceptable", and it would
  have refused real work: on one issue the maintainer rejected both options the triage offered,
  named a third, and explained that the triage's radius applied only to the two it proposed. No
  token vocabulary expresses that. Three findings stay binding — no reply at all, a reply from
  someone who may not decide, an issue an open PR already claims — because those are
  unambiguous and being wrong is expensive.
- **`verify-citations.sh` exists because models produce confident wrong line numbers** and are
  measurably poor at catching their own; asking more firmly does not fix it, and a mechanical
  check does. It also enforces the length ceilings, for the same reason.
- **`test-agent-scripts.sh` is not optional.** Every severe defect in these two scripts was
  invisible to careful reading: two separate code reviews read them and missed dead override
  parsing, a radius check that passed a marker with no radius, and adjacent empty TSV fields
  shifting a maintainer's decision into the wrong variable. All three fell out of *running*
  them. Run the tests after any change; each one fails if its fix is reverted.
- **The controlled vocabulary in `PROTOCOL.md` is load-bearing.** `board.sh` and the run
  summary read those tokens. A synonym is a bug, not a style choice. But a marker's `radius=`
  and `excluded=` describe the options the *triage* proposed — if a human chose a different
  design, the implementation stage re-assesses both rather than inheriting them.
- **`work=` selects the vocabulary, and most of the board is not `bug`.** Feature requests
  and extensions use the same two-commit evidence structure with `feat(`/`specify` instead of
  `fix(`/`reproduce`, and additive surface is not an excluded area — CLAUDE.md requires new
  shared types to live in `strom-types`, so scoring that as excluded would make the gate a
  rubber stamp.
- **`excluded=` means "would break", not "touches".** That distinction is what keeps gate 4 a
  real check.
- **The worked examples are the format spec.** They exist because rules describing a shape
  drift and an example does not. If you change the required shape, change the example in the
  same commit.
- **`SUMMARY.md` puts JSON before prose** so the human table cannot disagree with the record,
  and so a run's state survives without re-reading a long comment thread.

## Changing the protocol

Edit these files in a PR. The task definitions only need updating if the *dispatch* changes —
which file to read when — not when a rule inside a file changes.

Keep the instruction count per file low. Instruction-following degrades with the number of
simultaneous constraints, and the earlier a rule sits the more reliably it is obeyed; that is
why each file leads with its hardest rules and why the protocol is split at all.

## Running the scripts by hand

    REPO=Eyevinn/strom MAINTAINERS="alice bob" scripts/agent/board.sh

`MAINTAINERS` is the one variable that must be right: it lists the logins whose `/agent-fix`
reply may authorise work, and it defaults to a single login. If the deciding maintainer is not
in it, every armed issue is reported as "not a maintainer" and the queue looks correctly
empty. There is no `AGENT_LOGIN` — identity is by marker, not by account.

    scripts/agent/verify-citations.sh /tmp/review-body.md            # against HEAD
    scripts/agent/verify-citations.sh /tmp/review-body.md pr721       # against a ref
    gh api repos/Eyevinn/strom/pulls/725/reviews/<id> -q .body \
      | scripts/agent/verify-citations.sh - v725

Both need only `gh`, `git` and bash — no `jq`.

They are bash, not zsh. If you extract a function to poke at it interactively, run it under
`bash`; a zsh shell does not split unquoted expansions and some helpers will look broken when
they are not. `BOARD_LIB_ONLY=1 . scripts/agent/board.sh` sources the helpers without running
the report, which is how the tests reach them without touching the network.

Both were exercised against live repository data before landing. `verify-citations.sh` found a
real off-by-one in a shipped review (a call cited one line above where it is) and four
citations to bare filenames matching 2-24 tracked files each. `board.sh` was run against the
whole open board, where it turned up a PR whose *body* merely quoted a branch name being
reported as implementing an unrelated issue — this file's own PR, doing exactly that.

    scripts/agent/test-agent-scripts.sh
