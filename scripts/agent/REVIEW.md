# Reviewing a pull request

A real review: find what is wrong. More reviews catch more mistakes, so review every
non-draft pull request when its head moves. Unlike a triage comment, a review is about one
commit, so line references are right here.

## Which pull requests

- **Drafts:** skip, silently.
- **Dependency bumps:** skip.
- **Already reviewed at this head:** skip. Your review's marker carries the head SHA.
- **A new head that is only a rebase:** skip. Compare the patch, not the SHA:

      git fetch origin pull/<N>/head:pr<N> <old-head>
      git diff $(git merge-base origin/main <old-head>) <old-head> | git patch-id --stable
      git diff $(git merge-base origin/main pr<N>) pr<N> | git patch-id --stable

  Same id means the same change on a newer base. If the old head cannot be fetched, or the
  rebase had to resolve conflicts (the ids differ), review it.

Check out the branch to read it, but keep working from the `origin/main` copy of these files
the task took at the start of the run. The checked-out tree is the thing under review.

## Review it among its neighbours

A pull request is rarely alone. Before reading the diff, find the other open pull requests
and issues in the same area, by the files it touches and by subject
(`gh pr list --state open --search ...`, `gh issue list --state open --search ...`, and the
**Area:** line of triage comments), and read their titles and descriptions. The most useful
thing a review can say is often about the set, not the diff: two PRs changing the same path,
one that makes another unnecessary, an order they must land in, an open issue whose design
this one settles in passing.

**Stacks.** A pull request whose base is not `main` sits on another pull request. Review only
its own delta (`git diff origin/<base>...pr<N>`), name the stack in one line, and do not
repeat findings that belong to the PR below it.

## What to look for

Read the mechanism, not the description. In rough order of what matters:

1. **Is it the right fix?** Does the stated cause explain the symptom? Is it fixed at the
   right layer, or suppressed downstream? Does it close every path to the failure, or one?
   Has this area been patched repeatedly (`git log --oneline -- <path>`), which suggests the
   cause is elsewhere?
2. **What else does it touch?** Other blocks, pipeline shapes, platforms, native vs WASM.
3. **Lifetimes and teardown.** Anything that spawns a thread, arms a timer, installs a probe
   or captures a pipeline object: find where it is destroyed. CLAUDE.md's rules on closures
   and probes exist because these leak.
4. **Tests.** Does a test call the changed code, and would it fail if the fix were reverted?
   Does it need an element the CI image lacks (`.github/workflows/ci.yml`), so it skips and
   guards nothing? Read the check runs (`gh pr checks <N>`): zero runs is not green.
5. **CLAUDE.md and `docs/BLOCK_GUIDELINES.md`.** Quote the rule a diff breaks.

Every claim about how the code behaves carries a `path:line` you read at this head. If you
could not settle something, say so; do not guess. Mark a risk you have not traced as
"not verified".

## What to post

A GitHub review with event **COMMENT**, never APPROVE or REQUEST_CHANGES: the account you
post from may be a maintainer's own, and an approval from it reads as theirs.

- Put each finding as an inline comment on the line it is about.
- The review body: an **Area:** line as in `TRIAGE.md`, one line saying what the PR does
  and whether the approach looks right, then what you found about its neighbours, then the
  findings that do not fit on a line, numbered, most serious first. At most
  **1500 characters**.
- Found nothing? Say that in one line, plus what you checked. Do not pad.
- Last line of the body:

      <!-- strom-agent review head=<full head sha> -->

If the author or maintainer replies under your review, leave it. The next review happens
when the head moves.
