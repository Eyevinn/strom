# Turning an approved design into a draft PR

Read `PROTOCOL.md` first. This is the stage after a human answered a triage `Ask:`.

You never decide the design yourself, and you never merge.

A PR from you is only worth its review time if it is either **mechanically checkable** — a
test that fails without the fix and passes with it — or **honestly labelled as an unverified
proposal** with the exact experiment that would falsify it. A PR that looks verified and is
not is the one unrecoverable failure of this stage.

## Phase 1 — follow up on what you opened earlier

Do this first; it is cheap, and it is where most of the value of this stage accrues.

For every open draft PR authored by you:

1. Read its check runs (`gh pr checks <N>`). If both runs of the two-commit structure have
   concluded, edit the body to replace the pending evidence lines with the actual
   conclusions, quoting the failing line from the test-only run. Say explicitly that the red
   run is the proof the test guards the fix — a red check otherwise reads as a broken PR.
2. If commit 1 is red on the new test and commit 2 is green, **promote the PR from class B or
   C to class A**: change the verdict line, quote the failing line as evidence, switch
   `Refs #<N>` to `Fixes #<N>`, and drop a `[needs hardware]` title prefix if it was only
   ever waiting for CI. **That promotion is the point of this phase.**
3. If CI on the head commit is red for any other reason, fix it if the cause is in your own
   diff, or set the body's verdict to `BLOCKED` and say what you could not resolve.
4. If the PR is older than 14 days with no human comment, close it with a one-line comment
   saying it is being closed as an unverified proposal that went stale and that the issue
   remains open. Stale proposals that look like work in progress cost more than they are
   worth.

   **Exception — never close a PR that is only waiting for a dispatch.** macOS and Windows do
   not build on push or `pull_request`, and this stage may not fire a workflow run. So for a
   platform-specific fix the *only* route to class A runs through a human typing
   `gh workflow run ci.yml --ref <branch> -f platforms=windows`. Closing that work as stale
   destroys a correct PR for being blocked on someone else. Such a PR is `class=C`
   (`blocked-on-dispatch`, see below): instead of closing it, re-post the dispatch command as
   a comment and carry it in every run summary while the PR stays open. CI-configuration
   fixes are usually this shape — their whole guard *is* the dispatch log, not a Rust test.

## Phase 2 — pick at most one issue

Run the gate script rather than evaluating eligibility yourself:

    scripts/agent/gates.sh

It prints one eligible issue, or `NONE`, plus the first gate that stopped each candidate.
The gates it applies:

1. Carries a `protocol=v3 kind=triage` comment with `verdict=CONFIRMED` and an `Ask:`.
2. A **maintainer** replied **after** that comment using the answer syntax from
   `PROTOCOL.md`. Two things matter here and both have failed in practice: a comment is the
   agent's by its marker, never by its author, and a reply from someone who may not decide —
   an issue reporter, say — does not arm anything.
3. `radius=LOCAL` in the triage marker, or the answer carries `--accept-radius <that token>`.
4. `excluded=none` in the triage marker, or the answer carries
   `--accept-excluded <those tokens>`. The excluded areas are workspace dependencies, crate
   features and version floors; **breaking** changes to the API or WebSocket contract —
   renaming or changing an existing `StromEvent` variant, endpoint shape or config key;
   pipeline lifecycle, teardown and object references; BUFFER probes.

   *Additive* work is not excluded. CLAUDE.md requires new API-visible and shared types to
   live in `strom-types`, so treating an added type, variant or block as excluded would
   demand an override for the one placement the repo mandates — and a gate that fires on
   every feature is a rubber stamp, not a check. What is excluded is breaking or
   lifetime-critical surface. A new endpoint still needs both `#[utoipa::path]` and its
   `openapi.rs` registration; that is a repo rule to follow, not a gate to clear.

   These areas are excluded because they need a maintainer decision — so once that decision
   exists and says so explicitly, the exclusion has done its job.
5. No open PR already implements it.

**When an override armed the issue, quote the authorising line in the PR body.** Gates 3 and
4 exist to force a human decision; an override is that decision, and it belongs in the
artifact rather than only in a run log.

Gate 5 reports *which* PR claimed the issue. A link keyword in a body can be a passing
mention rather than an implementation, so if the named PR plainly does not implement this
issue, say so in the summary instead of treating the issue as taken.

The script also prints the issues where a human replied but nothing machine-readable came of
it, each with the exact reply that would arm it. Those are **not** eligible. Report them
verbatim and leave them; do not decide on anyone's behalf, and do not interpret an
approving-sounding sentence as an answer. That strictness is the point — a model that
interprets approval is a model that can interpret its way into it.

If nothing qualifies, open no PR. Say so in the summary and stop. **A quiet run is a correct
outcome and happens often.**

## Phase 3 — implement it, test first

Branch off current `main`, structured as exactly two commits:

    commit 1: the test alone, no implementation
    commit 2: the implementation

That order is not a style preference. It makes "this test guards this change" checkable by
anyone in ten seconds: check out commit 1, run the test, watch it fail. The exact branch name
and commit subjects depend on `work=` — see the table below.

### The vocabulary depends on `work=`, and most of the board is not `bug`

The two-commit evidence structure is the same for every shape of change. The words around it
are not, and using the `bug` words for additive work produces PRs that misdescribe
themselves — a real one shipped an `enhancement` on branch `agent/fix-691-…` titled
`fix(efpsrt): …` with a commit claiming to "reproduce" a defect that never existed.

Read `work=` from the triage marker and use the matching row:

| `work=` | branch | commit 1 | commit 2 | PR title |
|---|---|---|---|---|
| `bug` | `agent/fix-<issue>-<slug>` | `test(<scope>): reproduce <issue> (#<N>)` | `fix(<scope>): …` | `fix(<scope>): …` |
| `extension` / `feature` | `agent/feat-<issue>-<slug>` | `test(<scope>): specify <what> (#<N>)` | `feat(<scope>): …` | `feat(<scope>): …` |

For the additive row, **commit 1 may legitimately fail to compile**, because the API it calls
does not exist yet. That is still a red that turns green and it still proves the test
exercises the new code rather than restating it — but say so explicitly in the body, because
an unexplained compile error reads as a broken PR and a reviewer who assumes that will close
it.

Everything else — the classes, the required sections, the draft rule, the excluded areas — is
identical. Pick the row from `work=`, not from the issue's label.

The test must exercise the code it guards — call the changed module, do not rebuild the
behaviour inline — and it must fail if the fix is reverted. If you cannot write such a test,
say so in the PR body in those words and explain why a real guard is not feasible. Do not
ship a test that hardcodes the fixed path and call it a regression test.

If the test needs a GStreamer element, check the package list in `.github/workflows/ci.yml`.
A test that skips on a missing element passes green and guards nothing, so add the package in
the same PR or pick a different approach.

Then establish what you can actually verify. Try `cargo --version` and
`pkg-config --modversion gstreamer-1.0`:

- **If you can build:** run the tests at both commits and paste the real output — the failure
  at commit 1 and the pass at commit 2. Build from the workspace root, never with `-p`.
- **If you cannot build:** say that plainly once, and give the maintainer the two commands
  that reproduce the before and after locally.

Expect a formatting round trip. Without `cargo` you cannot run `cargo fmt`, and CI's
`Check formatting` step is unforgiving — match the surrounding style closely, and when that
check is the only thing red, fix it and **force-push, keeping exactly the two commits**.
Never add a third commit to repair a mechanical check.

Push commit 1, open the draft PR, then push commit 2. CI has no concurrency group, so both
runs complete and both appear on the PR: the first is the deliberate failure, the second is
the state you are proposing. Record both run URLs in the body; Phase 1 of a later run fills
in their conclusions.

## The PR

**Draft, always.** Title `fix(<scope>): <what it does>`, prefixed `[needs hardware]` when
class B applies for want of a device rather than for want of time.

- **Class A — verified.** You are holding execution evidence right now: test output you
  produced in this run, or two concluded CI runs showing commit 1 red and commit 2 green.
  The maintainer's job is to read the diff, not to reproduce anything.
- **Class B — proposal, not verified.** The evidence has not arrived yet, or cannot be
  produced here at all (no device, browser, GPU or network).
- **Class C — blocked on a dispatch.** The evidence exists and is one human command away:
  the covering check is a macOS or Windows build that this repo runs only on
  `workflow_dispatch`. Distinct from B because "no evidence yet" and "evidence that needs a
  human to fire it" call for different things from the reader, and because C is exempt from
  the 14-day stale closure. State the exact dispatch command in the body.

**A newly opened PR is therefore almost always class B**, because you open it before CI can
have concluded — or class C, when the covering check is a platform build only a human can
fire. That is not a defect in the work — it is the honest state of the evidence, and
Phase 1 of a later run promotes it. Never reason "the mechanism is obviously right, so this is
A": certainty is not evidence.

Required sections, in this order, omitting any with nothing to say:

1. **Verdict line.** One of:
   `Class A — the test below fails without this fix.`
   `Class B — proposal, not verified: <why>.`
   `Class C — blocked on a dispatch: <the command>.`
   Then `Fixes #<N>` for class A, `Refs #<N>` for B and C — work whose evidence has not
   arrived must not auto-close the issue.
2. **Problem.** The mechanism, cited.
3. **Change.** What you did and why this approach. The repo keeps no internals docs, so the
   PR trail is the design record. Name the alternative the triage stage rejected.
4. **Evidence.** Class A: the two commands and their real output, or the two CI run URLs with
   the deliberate first failure explained. Class B: what the argument rests on, and then, as
   its own paragraph, **How to falsify this** — the exact thing to run or watch, and which
   result means the hypothesis is wrong. *A proposal without a falsification test is an
   opinion and must not be opened as a PR.*
5. **Not verified.** Everything you did not run: platforms, other codecs and pipeline shapes,
   the zero/one/many cases, native versus WASM. State it even when the list is long — the
   maintainer scaling the work down is their call, not yours.
6. **Blast radius.** What else calls the changed symbol (grep and read at least one call
   site), and which configurations other than the reporter's change behaviour.

Then comment once on the issue linking the PR and naming its class. Do not restate the body.

## Before you publish, confirm

- `verify-citations.sh` exits zero on the body, if the body cites code.
- The branch has exactly the two commits, in that order.
- The PR is a draft.
- The class matches evidence you can point at. If you did not run the test yourself and CI
  has not concluded, it is B, and the body says `Refs`, not `Fixes`. The issue comment and
  the run summary repeat whatever class the body claims, so a premature A propagates to three
  places at once.
- Nothing in the diff touches an excluded area that the answer did not explicitly name.
  If an override authorised one, the body quotes the line that granted it.
- No emojis in log macros. Everything in English.
