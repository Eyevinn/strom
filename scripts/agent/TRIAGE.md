# Triaging an issue

Read `PROTOCOL.md` first.

An issue report is a claim about the code and gets verified the same way a PR does. Lead with
the finding — no "thanks for the detailed report" opener, and never merely restate the issue
back at the reporter.

## Verify against `main`

Locate the responsible module and read it, then say whether the reported behaviour follows
from the code as written.

Use history. The outcomes, in descending order of value to the reporter:

- **already fixed since their version** — cite the fix commit; this is the best outcome and
  the cheapest to check, so check it first;
- **a regression** — find the commit that introduced it;
- **a duplicate** — link the original;
- **long-standing behaviour** — say so plainly.

Check the docs for a mismatch. Check whether an existing test encodes the current behaviour,
because changing the behaviour means changing that test — name it if so.

## Reach exactly one verdict

One token from the triage-verdict row in `PROTOCOL.md`. If you cannot reach one, post nothing
and record `insufficient evidence, no comment` in the run summary.

The tokens are named for bug reports, but most of the board is not a bug report — it is
feature requests, extensions of something that exists, and capability questions. Read them
this way for those:

| Token | For a bug report | For a feature request or capability gap |
|---|---|---|
| `CONFIRMED` | the behaviour follows from the code | the capability genuinely is absent, and the code says where it would go |
| `NOT_REPRODUCIBLE` | the code does not produce this | the capability already exists by another route — name it |
| `ALREADY_ADDRESSED` | fixed since their version | shipped since their version, or an existing block already does it. **The highest-value outcome for a request** — check it first |
| `NEEDS_INFO` | cannot tell without more detail | the request is not yet specific enough to scope |

Then set `work=` in the marker: `bug` for wrong behaviour, `extension` for adding to
something that exists, `feature` for something new. The implementation stage reads it to pick
its branch, commit and title vocabulary, so a mislabelled shape ships a PR that misdescribes
itself.

## For a CONFIRMED issue, add a design proposal

Then **stop**. Do not implement it, do not open a PR, do not write more code than makes an
option concrete. A human decides the design before anything is built; that is the point of
this stage and where redirection is cheapest. A separate task implements what the maintainer
approves.

The proposal carries, in this order:

1. **Root cause** in a sentence or two, cited.
2. **Two options** with their trade-offs, including what each would break or complicate — or
   one option and why the obvious alternative is worse.
3. **Blast radius** of the recommendation. One token from the radius row. **Radius scores
   what the change modifies, not how much it adds** — a new block with no existing call sites
   is `LOCAL` however large it is, and calling it `SHARED` for its size locks it out of
   implementation for the wrong reason. Size belongs in the scope proposal below.
4. **The test that would guard the change**, and where it would live.
5. **Recommendation** in one line.
6. For `work=extension` or `work=feature`, a **scope proposal**: what PR 1 contains, and what
   becomes follow-up issues. A feature request is usually several PRs, and nothing else in
   this protocol cuts one into slices — so if you do not propose the cut, the maintainer
   writes it by hand or the implementation stage tries to build all of it at once.
7. **`Ask:`** — the question, answerable in one line.

The `Ask:` is the interface to the implementation stage, which will not act until a human
answers it. So:

- Make the options **distinguishable by name**, not by position. "Option A (single bus watch)"
  and "Option B (per-block guard)" can be answered; "the first one" cannot.
- Make the question closed. One line should settle it.
- State that `/agent-fix <option-name>` in a reply is the answer the implementation stage
  reads, and that any other phrasing needs a human to interpret it.

## Marker backfill — cheap, and not a re-triage

Triage comments written before the marker carried `verdict=` / `radius=` / `excluded=` /
`ask=` leave `board.sh` unable to say anything useful about an issue, and because a standing
v3 comment also suppresses re-triage they would otherwise stay that way. `board.sh` reports
them as notes against the candidate:

    #719   notes: triage marker predates the structured fields

For each one, post a short comment carrying **only** the marker, with a one-line note that it
restates the standing triage rather than replacing it:

    Marker backfill for my triage of 2026-08-27 — verdict and radius unchanged, no re-verification.

    To implement Option A, reply: /agent-fix A

    <!-- strom-agent protocol=v3 kind=triage issue=719 base=1c06c37 verdict=CONFIRMED work=bug radius=LOCAL excluded=none ask=open confidence=HIGH -->

That middle line matters more than it looks. Standing triages were written before the answer
syntax existed, so nothing on those issues tells a maintainer the token exists — and a
decision typed any other way cannot be read. The backfill is the only comment that will be
posted on them, so it has to carry the invitation. Where the marker records a non-`LOCAL`
radius or an excluded area, spell out the override the answer needs:

    To implement Option A, reply: /agent-fix A --accept-radius SHARED

Read the fields off your own standing comment. Do not re-derive them, do not re-verify, and
do not restate the design proposal — the original comment stays the record.

A backfill is one API call and no verification, so it **does not consume item budget**. Do at
most six per run. If your standing comment does not actually state a radius, or you cannot
tell which excluded areas the fix touches, the issue needs a real re-triage — leave it and
say so in the summary rather than guessing a token into the marker.

## When a standing triage must be re-read

A standing v3 comment normally suppresses re-triage. Two cases override that, because in both
the issue has moved and nothing else will ever notice:

1. **`verdict=NEEDS_INFO` and a human has commented since.** That comment is the answer to
   your own question. Re-triage from scratch — the new information very likely changes the
   verdict. Live example of the failure this prevents: an issue triaged `NEEDS_INFO` received
   a detailed research follow-up ninety minutes later, and four consecutive runs then
   reported "all open issues already carry a current v3 comment" without ever reading it.
2. **`ask=open` and a human replied without the answer syntax.** The implementation stage
   cannot act on that reply, but you are the stage that should read it and decide whether the
   design changed. If it did, publish a new proposal. If it did not, say so and restate the
   token to use.

Put both above "anything genuinely new" in your priority order. `board.sh` names the issues
in case 1 explicitly (`needs RE-TRIAGE, not a fix`) and lists case 2 under the
"a human replied" heading, so you do not have to hunt for them.

## Length

**The triage comment body is at most 2500 characters.** Count before posting; the check is
`scripts/agent/verify-citations.sh --max-chars 2500 <file>`.

That is a ceiling, not a target. The design proposal is the part worth its length — it is the
design record for work that has not been built yet. What has to go is the restatement: do not
summarise the issue back at the reporter, do not repeat what your own standing comment already
established, and do not pad the options with reasoning that does not change the choice.

## Labels

Only labels that already exist in this repo:

    bug  documentation  duplicate  enhancement  good first issue
    help wanted  invalid  question  wontfix  dependencies  rust

At most two, only where your verification justifies it. Never create one. Remove any you can
no longer justify.

## Worked example

---

**CONFIRMED.** `remove_bus_watch()` removes the bus *signal* watch rather than the per-block
handler, so the first block to stop takes the watch away from every other block on the same
bus, and GStreamer logs a CRITICAL on each subsequent stop.

`backend/src/gst/pipeline/bus.rs:212` — `` `bus.remove_watch().ok();` ``

The watch is added once per block at `bus.rs:141` — `` `bus.add_signal_watch();` `` — so the
add is refcounted per block while the remove is not, and the imbalance surfaces on the second
stop of any flow with two or more bus-listening blocks (Media Player, audio analyzer, mixer,
vision mixer).

**Option A — one watch for the bus.** Take the signal watch once in `setup_bus_watch` for the
pipeline's lifetime and let blocks attach handlers to it; remove it once in teardown.
Removes the imbalance rather than counting around it. Complicates nothing, but touches the
order in which handlers see messages, so a block that relies on being the only handler would
need checking — grep says none do.

**Option B — refcount the removes.** Keep the per-block add and track a count, removing the
watch only when it reaches zero. Smaller diff, but it leaves two mechanisms (GStreamer's own
refcount and ours) that have to stay in agreement, which is the class of bug being fixed.

**Radius** — `LOCAL`: one module, no contract surface, log-noise-only in observable impact.

**Guarding test** — `backend/tests/bus_watch_test.rs`: build a flow with two bus-listening
blocks, stop it, assert no CRITICAL reaches the log capture. Fails today on the second stop.

**Recommendation** — Option A. It removes the invariant instead of maintaining it.

**Ask:** Option A (single bus watch) or Option B (refcounted removes)? Reply
`/agent-fix A` or `/agent-fix B`; any other phrasing needs a human to interpret before the
implementation stage can act.

Confidence: HIGH

`<!-- strom-agent protocol=v3 kind=triage issue=719 base=1c06c37 verdict=CONFIRMED work=bug radius=LOCAL excluded=none ask=open confidence=HIGH -->`

---

## Before you post, confirm

- `verify-citations.sh` exits zero on your body. A marker backfill cites nothing by
  design, so check that one with `--allow-no-citations`.
- The marker carries `verdict=`, `work=`, `radius=`, `excluded=` and `ask=`, using vocabulary
  tokens. `board.sh` reports each missing one as a note the implementation stage has to
  answer in its PR body, so an incomplete marker turns into work for the next reader.
- `excluded=` names what the change would **break**, not what it touches. Adding a type to
  `strom-types` is `none` — CLAUDE.md requires it to go there.
- The `Ask:` names its options and can be answered in one line.
- Labels are from the list above, at most two.
