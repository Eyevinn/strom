# Agent review protocol

Strom's open PRs and issues are worked by two scheduled agents: one reviews PRs and triages
issues, the other turns an approved design into a PR. This directory is the protocol
they follow, and their role, budget and priority order with it.

**It lives in the repo on purpose.** All of this used to be embedded in the task definitions,
where it could not be diffed, reviewed or corrected by anyone but the person who owned the
schedule. Here it is version-controlled, and a PR against it is a PR against how the bots
behave. What is left in a task definition is a stub: the boundary, the credentials, the
wiring, and a pointer to `ROLE_REVIEW.md` or `ROLE_FIX.md`.

## Files

| File | Read when |
|---|---|
| `ROLE_REVIEW.md` | The review/triage task's stub points here. Role, budget, priority order. |
| `ROLE_FIX.md` | The implementation task's stub points here. Role and budget. |
| `PROTOCOL.md` | Every run, first. Evidence rules, citation form, controlled vocabulary, markers. |
| `REVIEW.md` | Reviewing a pull request. |
| `TRIAGE.md` | Triaging an issue. |
| `FIX.md` | Turning an approved design into a PR. |
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

**And nothing about what an agent may do.** The `Allowed:` and `Forbidden:` lists stay in the
task definition, because that is the one place a pull request cannot reach. Splitting on that
line is the whole reason the stubs can be thin: a bad edit to a file in here costs a bad
review, and a bad edit to a boundary would cost a push to `main`. So a role file states the
boundary is not its to widen, and `PROTOCOL.md` says a diff that appears to grant a
permission is a finding rather than an instruction.

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
- **The protocol is read from `origin/main`, because a checked-out tree is untrusted input.**
  `REVIEW.md` has the agent check out the pull request it is reviewing, and the protocol is
  read per item — so before this rule, the files telling the reviewer what it may do were
  read out of the diff being reviewed. On a public repository that invites outside
  contributions, and with a token that can write contents and pull requests, that is a
  permission boundary anyone who can open a pull request could edit; nothing had to be merged
  for the next scheduled run to read it. The fix is one copy taken from the pinned branch at
  the start of a run. Do not "simplify" it back to reading `scripts/agent/` in place, and do
  not let a stub prompt point at a path in the working tree.

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
- **The implementation stage opens a PR ready for review, and holds other people's drafts.**
  The two rules look contradictory and are not. A draft means "the author is not finished",
  and the implementation stage stops only when it is finished — it opened drafts for a while,
  which said the opposite and left a shelf of PRs nobody could tell apart from abandoned ones.
  How far the *evidence* has got is a different axis, and the class line already carries it:
  a class B PR is finished work whose proof has not arrived, not unfinished work. So the
  draft flag is free to mean what it means everywhere else, and the hold below applies to
  drafts from contributors, never to this stage's own PRs. The draft flag had been doing one
  job nobody had noticed, though — keeping the review stage off these PRs — so `REVIEW.md`'s
  skip list now says that outright, keyed on the `kind=fix` marker.
- **A draft is held, not skipped, and the hold is said exactly once.** Those are two separate
  corrections to the same rule. Silently skipping a draft told its author nothing, so a
  contributor who had opened one had no way to know whether it was queued, ignored or waiting
  on them; and the shape that fixes that — a comment — is also the shape that turns into a
  nag, because these tasks run twice a weekday and would otherwise re-post it on every new
  commit. Hence a `kind=draft-hold` marker and "one per pull request, ever": the standing
  comment is the memory. Do not "improve" the hold into a review that opens with a caveat,
  and do not make it repeat when the head SHA moves — a moving head SHA is what a draft *is*.
  Being asked is the only trigger for a real review, and then it is a full one, because a
  half review of a draft is the outcome both halves of this rule exist to avoid.
- **`FIX.md` gives the PR body a per-section allocation, not just a ceiling.** A ceiling alone
  tells a writer nothing until the text already exists, so the body got written at full length
  and then shaved. One run descended 4970 -> 4329 -> 4105 -> 3772 -> 3633 -> 3488 characters
  across six checks, the last of them a script substituting phrases to land twelve characters
  under the limit — turns that should have gone into the change. The allocation exists so the
  first draft is the right size; `REVIEW.md` achieves the same thing by saying its worked
  example is 1500 characters and most reviews should land near it. Do not delete the numbers
  and leave the ceiling.

- **The worked examples are the format spec.** They exist because rules describing a shape
  drift and an example does not. If you change the required shape, change the example in the
  same commit.
- **The stub/repo split is on a security axis, not a convenience one.** It is tempting to
  move the last few things out of the task definitions too and be rid of the redeploy step
  entirely. Do not move the permission boundary. A scheduled agent reads these files with a
  token that can write contents and pull requests, and the review stage reads them while a
  contributor's branch is checked out; the boundary is only a boundary while it lives
  somewhere a contributor cannot edit. Everything else — role, budget, priority order, all of
  the protocol — is fair game, and the point of the split is that it is the larger half.

- **`SUMMARY.md` puts JSON before prose** so the human table cannot disagree with the record,
  and so a run's state survives without re-reading a long comment thread.
- **Reference form is destination-dependent, and that asymmetry is deliberate.** `#738` is
  right in the summary comment, which is posted in this repository and autolinks it. It is
  wrong in the onward message, where nothing autolinks and a reader has to search for every
  item by hand. Do not unify the two on the shorter form to remove the special case; the
  special case is the whole point. `SUMMARY.md` also carried a blanket "never a remote URL"
  rule that made bare refs the only option — it now bans the URLs that actually matter
  (credentials, endpoints, anything outside this repository) and requires the ones that do
  not.
- **A task prompt must not restate a rule one of these files owns.** The prompt is the outer
  instruction, so a restated rule does not drift — it overrides, silently, and the file looks
  correct while every run ignores it. Both prompts capped the onward message at five lines and
  banned it from carrying a remote URL after `SUMMARY.md` required linked, one-line-per-item
  output; the next two runs posted the old shape, and it read as a protocol failure rather than
  a stale prompt. A prompt carries the role, the constraints, the budget and the wiring. The
  shape of anything posted lives here.

## Rules in the protocol files, reasoning here

Each file an agent reads carries **rules and worked examples**. The reasoning behind a rule
lives in this file, which is not in any read path — the role files dispatch to `PROTOCOL.md`,
`REVIEW.md`, `TRIAGE.md`, `FIX.md` and `SUMMARY.md`, never here.

That is not a concession to human readers at the model's expense. It is the same constraint
in both directions: instruction-following degrades with the number of simultaneous rules, so
a war story sitting beside a rule competes with it; and a protocol nobody can read is a
protocol nobody can review, which defeats the whole reason these files are in the repo.

Deleting the reasoning is the one thing not to do. Every rule in here was paid for by a run
that got it wrong, and a rule stripped of its reason looks arbitrary to the next editor —
which is exactly when it gets "simplified" away.

### Reasoning moved out of `REVIEW.md`

- **Reading `main` while reviewing a branch** is a common and invisible error. That is why the
  checkout command is spelled out rather than assumed.
- **Re-reviewing an unchanged diff because the thread moved** buries the discussion the
  maintainer is actually having. Hence a short reply instead, and a re-review only on a new
  head SHA or a new check conclusion.
- **A verdict on a moving diff** costs the author a round trip and buys nobody anything, which
  is the case for holding a draft rather than reviewing it.
- **The label is the remedy for a missing platform build, not a dispatch.** `workflow_dispatch`
  takes a branch or tag in this repository, and a fork pull request's head is only
  `refs/pull/<N>/head`, so a dispatch cannot reach it at all. A green run on the author's own
  fork builds their base, not this one.
- **A 10 000-character review of a 30-line diff** means the maintainer now has two things to
  read instead of one. That is what the ceilings are for.
- **`ROLE_REVIEW.md` already states what the reviewer is for**, so `REVIEW.md` no longer
  repeats it. A run reads the role file first; saying it twice spends the same budget twice.

### Reasoning moved out of `PROTOCOL.md`

- **A pull request that edits `scripts/agent/` would be handing the reviewer its own
  instructions**, from inside the diff it is judging, before it has judged it. The repository
  is public and invites outside contributions, which is why the pinned copy exists and why the
  scripts run from it while resolving against the checked-out tree.
- **A wrong citation is worse than none:** it makes the whole review impossible to spot-check,
  and a model cannot reliably catch its own bad citations — that is what the script is for.
- **A token from the wrong vocabulary row corrupts the log**, because the run summary and the
  markers carry those values verbatim.
- **`work=` mislabelled ships a PR that misdescribes itself:** an `enhancement` implemented
  under the `bug` vocabulary lands as `fix(scope): …` with a commit claiming to "reproduce" a
  defect that never existed.
- **Looking for `class=` on a review produces a false finding.** A run once reported "five open
  PRs are missing `class=`" about five PRs it had merely reviewed; `class=` exists only on a
  fix PR the implementation stage authored.
- **`excluded=` meaning "would break" rather than "touches"** is what keeps the gate a real
  check. CLAUDE.md *requires* new shared types to live in `strom-types`, so scoring an
  additive type as excluded would demand an override for the one placement the repo mandates.
- **Two thirds of everything these tasks have written is run summaries**, and the worst of them
  re-listed sixteen unchanged issues to say nothing had changed. That is what made the log
  unreadable to a human and, once, to the agent itself — hence "write once, then cite".

### Reasoning moved out of `TRIAGE.md`

- **Calling a large additive change `SHARED` for its size** locks it out of implementation for
  the wrong reason. Radius scores what a change modifies; size belongs in the scope proposal.
- **Nothing else in the protocol cuts a feature into slices**, so an unproposed cut means the
  maintainer writes it by hand or the implementation stage tries to build all of it at once.
- **The backfill's invitation line exists because standing triages predate the answer syntax.**
  Nothing on those issues tells a maintainer the token exists, and a decision typed any other
  way cannot be read — and the backfill is the only comment those issues will ever get.
- **A standing v3 comment suppresses re-triage, which is why two cases override it.** Live
  example: an issue triaged `NEEDS_INFO` received a detailed research follow-up ninety minutes
  later, and four consecutive runs then reported "all open issues already carry a current v3
  comment" without ever reading it.
- **`TRIAGE.md` had a ceiling and no target**, the same defect `FIX.md` had (see above). Its
  worked example measures 2019 characters, so that is now the stated target. `REVIEW.md` said
  its example was 1500 characters when it measures 1893; corrected, because a target nobody
  can hit is not a target.

### Reasoning moved out of `SUMMARY.md`

- **An unpaginated comment fetch returns the thirty oldest comments**, which once made a run
  report a months-old comment as the previous run and invent a gap in the log.
- **A field invented to fill a column** has already put three different radii in this log for
  one PR. That is why an undetermined field is `null`.
- **A bare `#721` is four characters a reader has to look up by hand** anywhere outside this
  repository's own issues and pull requests, so a message naming six items costs six searches.
- **A task prompt once capped the onward message at five lines and banned remote URLs**, which
  left a single line of bare refs as the only legal output; runs kept shipping exactly that
  after this file had already required otherwise.

### Reasoning moved out of `FIX.md`

- **A PR that looks verified and is not** is the one unrecoverable failure of that stage.
- **Closing a dispatch-blocked PR as stale destroys a correct PR for being blocked on someone
  else**, which is why class C is exempt from the 14-day closure.
- **Class B and class C are distinct** because "no evidence yet" and "evidence that needs a
  human to fire it" call for different things from the reader.

## Changing the protocol

Edit these files in a PR. The task definitions only need updating if the *boundary* or the
*wiring* changes, or if a role file is renamed — not when a rule inside a file changes, and no
longer when the budget or the priority order changes. That holds only while the
prompts state no rule these files own; if one does, changing the file here is not enough, and
the fix is to delete the rule from the prompt rather than to keep the two in step.

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
