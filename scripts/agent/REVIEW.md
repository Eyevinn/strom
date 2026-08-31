# Reviewing a pull request

Read `PROTOCOL.md` first. This file covers one PR, start to finish.

Incoming PRs come from someone who hit a real problem, fixed it, and confirmed their own
symptom is gone — take that at face value. Your value is the two questions they could not
answer: **is it the right fix, and what else does it touch.**

Make the merge decision a short read. A review that takes longer to read than the diff has
failed, however correct it is.

## Check out what you are actually reviewing

    git fetch origin pull/<N>/head:pr<N> && git checkout pr<N> && git diff origin/main...pr<N>

Reading `main` while reviewing a branch is a common and invisible error.

## Skip these

- Dependency version bumps — but still dismiss any older-generation review of yours on them.
- A PR whose current head SHA already carries your v3 review. Only a new head SHA or a new
  check conclusion is a re-review trigger.
- Drafts, unless the body asks for review.

A review is a verdict on a diff, not a turn in a conversation. The maintainer and the author
will keep talking under your review; that discussion is not addressed to you. If a comment
directly contradicts something your standing review concluded, post a short reply — under
1000 characters, naming only what changed and what it does to the verdict — and do not
re-review. Re-reviewing an unchanged diff because the thread moved buries the discussion the
maintainer is actually having.

## Work these six, in order

1. **Claims.** Extract each checkable claim, verdict it from the claim-verdict row in
   `PROTOCOL.md`. The PR's own "Verification" section is a claim, not evidence. Table only
   claims that could change the decision.

2. **Diagnosis — right fix, or moved symptom?** Read the mechanism; do not accept the
   description's account of it. Does the stated cause explain every reported symptom? What
   else produces the same symptom, and does this cover those paths too — a fix that closes
   one trigger is worth landing, but name the rest. Is coverage `ABSOLUTE` or `BOUNDED`
   (state the bound)? Right layer, or suppression downstream? Has this area been patched
   repeatedly (`git log --oneline -- <path>`), which suggests the root cause is elsewhere?

   For anything adding a request, retry, reconnect or repeated event: cost the *response*,
   not the request. Say whether the loop is OPEN (fires a fixed number of times regardless
   of outcome) or CLOSED (stops once the desired state is observed), and for an OPEN loop
   what it costs on the healthy path — the sessions where the condition being fixed is
   absent, which is most of them.

3. **Blast radius.** One token from the radius row. Grep the changed symbols for call sites
   and read at least one. Additive code has no callers but still has a lifetime: name what
   creates it, what destroys it, whether the destroy path can run concurrently, and the
   overlap window in wall-clock terms — anything spawning a thread, arming a timer,
   installing a probe or taking a reference to a pipeline object needs its teardown path
   found. "Confined to the file the diff touches" describes the diff, not the radius.

   Say which configurations other than the reporter's change behaviour: other blocks,
   pipeline shapes, codecs, containers, native vs WASM, CPU vs GPU, other platforms, and the
   zero/one/many cases of any count property. `SHARED` or `GLOBAL` raises the bar — say what
   would break and how it would show up.

4. **Tests and CI.** Read the actual check runs (`gh pr checks <N>`).

   **Zero check runs is not green — it is no evidence, and it blocks approval.** Look for a
   run stuck awaiting a maintainer (`gh api "repos/Eyevinn/strom/actions/runs?status=action_required"`)
   and give them the command: `gh api -X POST repos/Eyevinn/strom/actions/runs/<run_id>/approve`.

   A green run is not evidence the change was tested. Determine whether the covering tests
   actually executed: look for skip guards (missing element, absent hardware, env gate) and
   cross-check the package list in `.github/workflows/ci.yml`. A test that skips silently
   passes green and guards nothing — that is a finding and a CLAUDE.md violation. Does a
   claimed test call the changed module, or rebuild the behaviour inline, and would it fail
   if the fix were reverted? Name any canary that should have run:
   `pipeline_lifecycle_test.rs` for new GStreamer elements or closures, the openapi snapshot
   for API types.

   macOS and Windows do not build on push or pull_request. Platform-specific code is
   therefore `UNVERIFIED`, with the command for the maintainer:
   `gh workflow run ci.yml --ref <branch> -f platforms=macos` (or `windows`, or `both`).

5. **Repo rules.** Check CLAUDE.md and quote any rule violated: BUFFER probe constraints,
   `WeakRef` instead of strong refs to pipeline/element/bin in closures, queue properties
   left at defaults, shared types belonging in `strom-types`, endpoints needing both
   `#[utoipa::path]` and `openapi.rs` registration, no blanket `dead_code`, no emojis in log
   macros, English only, and the Tests rules.

   The rules are proxies for properties. When a diff sidesteps the letter of one — holding a
   `Pad` rather than an `Element`, say — answer the underlying question instead, which is
   what this object's lifetime is relative to the pipeline's.

6. **Design record.** The repo deliberately keeps no internals docs, so the review trail is
   it. If the PR body does not say why this approach and what was rejected, write that
   reasoning into your review.

## Verdict — mechanical, not a judgement call

`Approve` requires **all** of:

- every claim about code in this repository is `CONFIRMED`;
- every remaining row is `EXTERNAL` with its assumption stated;
- CI has run and is green, with the covering tests actually executed;
- radius is `LOCAL`, or `SHARED` and explicitly argued.

A single `UNVERIFIED` or `CONTRADICTED` row, or zero check runs, means you may not approve.
Do not approve and then add caveats — if you want to, the verdict is `Comment`.

`Request changes` for: a `CONTRADICTED` claim, a red check belonging to the diff, a test that
cannot run in CI or does not guard the change, or a CLAUDE.md violation.

Otherwise `Comment`.

A run where everything is approved and nothing questioned is evidence that verification did
not happen.

## Shape and ceilings

Open with the verdict, then any numbered requested changes — each the smallest concrete
change that closes the gap, naming the file and what to add. Then the evidence.

**Omit any section with nothing worth saying.** A heading with "none" under it is noise.

- Body: **at most 4000 characters.** Count before submitting.
- Claim table: **at most 5 rows** — only claims that could flip the verdict.

These are ceilings, not targets. If the evidence does not fit, cut evidence rows, never the
verdict or the requested changes. A 10 000-character review of a 30-line diff means the
maintainer now has two things to read instead of one. A gap you found and then excused is a
finding wasted.

## Worked example

Match this shape. It is 1500 characters; most reviews should land near it.

---

**Verdict: Comment** — correct mechanism, but coverage stops short of two other pipelines
that can autoplug the same element, and the new platform code has never been compiled by
this repo's CI.

**Requested changes**

1. Call `rtp_hdrext::install()` on Media Player's internal pipelines too —
   `backend/src/blocks/builtin/mediaplayer/bridge.rs`, both constructors, before the
   returned pipeline can reach `Playing`.

**Claims**

| Claim | Verdict | Evidence |
|---|---|---|
| Covers every depayloader the process runs | `CONTRADICTED` | `backend/src/gst/rtp_hdrext.rs:168` — `` `for element in pipeline.iterate_recurse()` `` only walks the pipeline passed in; `bridge.rs:28` constructs a second `gst::Pipeline` that is never passed to `install()` |
| The `v1_22` floor rules out a feature bump | `CONFIRMED` | `Cargo.toml:29` — `` `gstreamer = { version = "0.23", features = ["v1_22"] }` `` |
| macOS/Windows FFI path verified by CI | `UNVERIFIED` | `gh pr checks 721`: `Build (macOS)` and `Build (Windows)` both `skipping`; the green run cited in the body is on the author's fork, not this repo |

**Diagnosis** — Root cause matches the issue's own trace (interrupted FU-A with contiguous
sequence numbers on a depayloader whose aggregation cache is populated). Coverage is
`BOUNDED` to pipelines the code calls `install()` on, and that is one of at least three that
host elements capable of autoplugging an RTP depayloader.

**Radius** — `GLOBAL`: three independent `gst::Pipeline` hosts, and the install runs during
pipeline construction.

**Tests & CI** — `Check (Linux)`, `Build (Linux x86_64/ARM64)`, `Check & Build (WASM)`,
`API Contract Check` green at `55e91ef`. Dispatch before merge:
`gh workflow run ci.yml --ref <branch> -f platforms=both`.

Confidence: HIGH

`<!-- strom-agent protocol=v3 kind=review pr=721 head=55e91ef... verdict=Comment radius=GLOBAL confidence=HIGH -->`

---

## Before you submit, confirm

- The PR number and title match the API response you just fetched.
- `verify-citations.sh` exits zero on your body.
- You read the check runs, and distinguished "zero checks ran" from "checks passed".
- The marker says `protocol=v3` with the correct head SHA and vocabulary tokens.
- Body is under 4000 characters.
