# Agent protocol v3 — shared core

Read this once per run, before the item-specific file. Everything here applies to reviews,
triage and fix PRs alike.

This file describes *what good output looks like*. It says nothing about where the agent
runs, what credentials it holds or where it reports — that belongs to the task definition
that points at this file.

## The four rules that outrank everything else

1. **Never claim you ran, built or tested something you did not.** If you write "verified",
   point at output you produced in this run. This is the one unrecoverable error.
2. **Every claim about how this code behaves carries a citation.** No citation, no verdict —
   publish `Confidence: LOW` and the specific questions instead.
3. **One verdict, stated once.** Never write "approve" in a body whose verdict is Comment.
4. **Uncertainty is a finding.** Prefer "I could not determine X" over a plausible guess.

## Citations

| Kind | Form |
|---|---|
| CODE | `` `path:line` `` followed on the same line by the line quoted verbatim in backticks |
| HISTORY | commit SHA plus what it changed |
| TEST | file path plus test name |
| CI | check-run name plus conclusion, quoting the failing line |
| DOCS | file plus heading |

Write CODE citations in this shape, because it is machine-checkable:

    `backend/src/gst/rtp_hdrext.rs:142` — `pub fn is_enabled(depay: &gst::Element) -> Option<bool> {`

Before you post anything containing citations, verify them:

    scripts/agent/verify-citations.sh <file-with-your-body> [git-ref]

It resolves every `` `path:line` `` against the tree and exits non-zero if a path is missing,
a line number is past end of file, a bare filename is ambiguous, or a quoted line does not
match. Fix what it reports. It also fails a body that cites nothing at all — pass
`--allow-no-citations` for the rare body where that is correct, such as a marker backfill.
A range citation (`path:N-M`) is bounds-checked only; a single-line citation is a verbatim
claim and its quote must match.
Do not paste its output into your review — it is a check, not evidence.

Re-read the file at the line immediately before citing it. Never estimate a line number
from a grep offset or a diff hunk header. A wrong citation is worse than none: it makes the
whole review impossible to spot-check, and a model cannot reliably catch its own bad
citations — that is what the script is for.

CODE is authoritative over DOCS. If they disagree, say so and cite both.

## Controlled vocabulary

Every field below takes **exactly one** token from its row. Never a pair, never a token from
another row, never an invented one. These are the values the run summary and the markers
carry, so a token from the wrong row corrupts the log.

| Field | Allowed values |
|---|---|
| Claim verdict | `CONFIRMED` `CONTRADICTED` `UNVERIFIED` `EXTERNAL` |
| Review verdict | `Approve` `Comment` `Request changes` |
| Blast radius | `LOCAL` `SHARED` `GLOBAL` |
| Fix coverage | `ABSOLUTE` `BOUNDED` |
| Triage verdict | `CONFIRMED` `NOT_REPRODUCIBLE` `ALREADY_ADDRESSED` `NEEDS_INFO` |
| Confidence | `HIGH` `MED` `LOW` |
| Fix PR class | `A` `B` `C` |
| Work shape | `bug` `extension` `feature` |

Two rows are routinely confused. **Blast radius** answers "how many call sites and
subsystems does this touch" — it is never `BOUNDED`. **Fix coverage** answers "does this
close the whole class of failure or one trigger" — it is never `LOCAL`. A diff can be
`LOCAL` and `BOUNDED` at once; they are different questions.

## UNVERIFIED vs EXTERNAL

- `UNVERIFIED` — settleable from this repository and not settled. **Blocks approval.**
- `EXTERNAL` — not settleable from this repository at all (upstream GStreamer, Axum, egui,
  the OS). Does not block, provided you state the assumption.

Never blend them: a hybrid always resolves to the permissive reading. Whether *this* repo's
teardown path can race *this* diff is `UNVERIFIED`, however much upstream behaviour it also
depends on.

Before writing `UNVERIFIED`, name the file that would settle it and read it. Lifecycle,
ordering, concurrency and teardown questions are almost always answerable statically, and
the code that tears a thing down is usually in a different file from the diff.

Any risk you have not traced in the code gets the literal prefix `SPECULATIVE (not verified):`.

## Markers

Every posted review, triage comment and fix PR body ends with a marker. The keys are read by
tooling, so spell them exactly and put them on one line.

    <!-- strom-agent protocol=v3 kind=review pr=721 head=<full-sha> verdict=Comment radius=GLOBAL confidence=HIGH -->
    <!-- strom-agent protocol=v3 kind=triage issue=719 base=<sha> verdict=CONFIRMED work=bug radius=LOCAL excluded=none ask=open confidence=HIGH -->
    <!-- strom-agent protocol=v3 kind=fix issue=719 pr=730 class=B -->

`protocol=v3` is the generation gate and must not change — anything without it counts as an
older generation and gets redone. The other keys are additive; a reader that finds one
missing treats it as unknown rather than assuming a value.

`work=` is the shape of the change, and it selects the vocabulary and the exclusion rules the
implementation stage uses. `bug` fixes wrong behaviour; `extension` adds to something that
exists; `feature` adds something new. Most of the board is not `bug`, so getting this wrong
mislabels the work: an `enhancement` implemented under the `bug` vocabulary ships as
`fix(scope): …` with a commit claiming to "reproduce" a defect that never existed.

`excluded=` lists the areas from `FIX.md`'s exclusion gate that the fix would **break or take
a lifetime risk in**, comma-separated, or `none`. It is not a list of areas the diff merely
touches, and the difference decides whether the gate is a real check or a rubber stamp:
CLAUDE.md *requires* new shared types to live in `strom-types`, so scoring an additive type
as excluded would demand an override for the one placement the repo mandates. Adding a type,
a variant or a block is `none`; renaming or changing an existing `StromEvent` variant, an
endpoint's shape or a config key is `contract`. `ask=open` means a design question is waiting for a
human; `ask=none` means no decision is needed.

## Do not redo settled work

Never rebuild a claim row your own standing review already verdicted **at this same head
SHA**. Cite it — "CONFIRMED in my review of <date>" — and spend the budget on what is new.

Re-verify from scratch only when the head SHA changed, or when something has since
contradicted that row. New comments on a thread are not a re-review trigger.

## The answer syntax

A design proposal stops for a human decision. That decision is only machine-readable in one
form, and `gates.sh` accepts nothing else:

    /agent-fix <option-name>

Two optional flags let a maintainer authorise work the gates would otherwise refuse. Each
must name the **exact** token from the triage marker, so an override is deliberate rather
than a shrug:

    /agent-fix A --accept-radius SHARED
    /agent-fix A --accept-excluded api,strom-types

An override is authorisation, so the fix PR body must quote the line that granted it. That
keeps the escape hatch visible in the artifact instead of buried in a run log.

Anything else — "do option A", "confirming option 1", a thumbs-up — is a real decision that
the tooling cannot read. It is surfaced for a human, never acted on. The gate is strict on
purpose: a model that interprets approval is a model that can interpret its way into it.

## Identity, and why it is not the author

The tasks may run under the same GitHub account a human uses. **Never decide whether a
comment is the agent's by looking at its author.** A comment is the agent's if and only if it
contains `<!-- strom-agent`. Keying on the login discards that human's decisions silently,
which is the worst failure shape available: the queue looks correctly empty.
