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
| `gates.sh` | Deciding whether any issue is eligible for implementation. |
| `verify-citations.sh` | Before posting anything that cites code. |

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
- **Both scripts exist because a prompt cannot do their job reliably.** `gates.sh` is boolean
  logic over API data — deterministic, so it does not belong in a prompt competing for
  attention. `verify-citations.sh` exists because models produce confident wrong line
  numbers and are measurably poor at catching their own; asking more firmly does not fix it,
  and a mechanical check does.
- **The controlled vocabulary in `PROTOCOL.md` is load-bearing.** `gates.sh` and the run
  summary read those tokens. A synonym is a bug, not a style choice.
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

    REPO=Eyevinn/strom AGENT_LOGIN=<bot account> scripts/agent/gates.sh

    scripts/agent/verify-citations.sh /tmp/review-body.md            # against HEAD
    scripts/agent/verify-citations.sh /tmp/review-body.md pr721       # against a ref
    gh api repos/Eyevinn/strom/pulls/725/reviews/<id> -q .body \
      | scripts/agent/verify-citations.sh - v725

Both need only `gh`, `git` and bash — no `jq`.

They are bash, not zsh: `gates.sh` word-splits `$MAINTAINERS` nowhere and quotes everything,
but if you extract a function to poke at it interactively, run it under `bash`, or a zsh shell
will not split unquoted expansions and `is_maintainer` will look broken when it is not.

Both were exercised against live repository data before landing. `verify-citations.sh` found a
real off-by-one in a shipped review (a call cited one line above where it is) and four
citations to bare filenames matching 2-24 tracked files each. `gates.sh` was tested against
the whole open board plus a reconstructed quoted-email reply, which is the case that could
otherwise have armed the implementation stage with a design nobody chose.
