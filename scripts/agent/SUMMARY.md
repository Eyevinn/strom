# Run summary contract

Read `PROTOCOL.md` first.

The summary is the only durable trace of a run. It is also the thing a human reads most
often, and the thing that has drifted most — so its format is fixed here rather than
described.

Post it as one comment on the issue titled **"Agent triage log"**. Create that issue once if
it is missing, with a body explaining it is the log for these scheduled tasks. Create nothing
else.

## Read state from the issue body, not the comments

That thread is long. **`gh api repos/Eyevinn/strom/issues/<N>/comments` without `--paginate`
returns the thirty OLDEST comments**, which has already caused a run to report a months-old
comment as the previous run and invent a gap in the log. Never read state that way.

State lives in the issue **body**, which this task owns and rewrites. Read it first, and
rewrite it last, replacing the block between the markers:

    <!-- state:begin -->
    Last triage run: 2026-08-31T08:00Z, main@1c06c37, 4 items
    Last fix run:    2026-08-31T10:00Z, opened #729 for #661 (class B, CI pending)
    Open agent PRs:  #700 (class A, green), #730 (class B, CI pending)
    Awaiting an answer: #719 #703 #702 #694 #690 #674 #673
    <!-- state:end -->

If you need history beyond that, `gh api ... --paginate` and read the **tail**.

## Emit the JSON block first

Begin the comment with one fenced `json` block against this schema. Everything a human reads
is rendered from it, so the two can never disagree.

    ```json
    {
      "task": "triage",
      "at": "2026-08-31T08:00Z",
      "base": "1c06c37",
      "worked": 4,
      "cap": 4,
      "needs_human": [
        {"ref": "#721", "what": "platform builds never compiled the new FFI path",
         "command": "gh workflow run ci.yml --ref wagenet/685-hdrext -f platforms=both"}
      ],
      "reviews": [
        {"pr": 721, "head": "55e91ef", "verdict": "Comment", "radius": "GLOBAL",
         "confidence": "HIGH", "ci": "linux green; macos/windows skipped",
         "supersedes": 5049165959, "dismissal": "not dismissable (COMMENTED)"},
        {"pr": 725, "head": "00ec80f", "verdict": "Approve", "radius": "SHARED",
         "confidence": "HIGH", "ci": "green, new tests executed in Check (Linux)",
         "supersedes": null, "dismissal": null}
      ],
      "triage": [],
      "fix": [],
      "skipped": [
        {"ref": "#726", "reason": "draft, body does not ask for review"},
        {"ref": "#698", "reason": "dependency bump, no stale review of mine"}
      ],
      "unfinished": [
        {"ref": "#700", "reason": "left by the 4-item cap"}
      ],
      "queue": {"candidates": 6, "claimed": 1, "awaiting_reply": 8, "untriaged": 1},
      "notes": []
    }
    ```

### Three rules that make the log trustworthy

1. **An item gets an entry only if you posted something about it this run.** Everything else
   goes in `skipped` as one `{ref, reason}` line. Never a row per untouched item — a summary
   that enumerates sixteen issues to say nothing changed is why nobody reads it.
2. **Every value is copied from what you actually posted this run.** Never restate a standing
   verdict's attributes from memory. If you did not determine a field, it is `null` — a field
   invented to fill a column has already put three different radii in this log for one PR.
3. **Vocabulary fields take vocabulary tokens** (`PROTOCOL.md`). `verdict`, `radius`,
   `confidence`, `class` are validated by tooling; a token from the wrong row is a bug.

`needs_human` is first in the schema because it is first in importance. Each entry carries the
exact command where there is one. If it is empty, that itself is the useful signal.

Two things belong in `needs_human` **every run**, not just the run that discovered them, because
otherwise they scroll away and the work stalls silently:

- the dispatch command for every open `class=C` PR **that the implementation stage authored**
  (`gh pr list --author @me --draft`), which cannot reach class A without it. A review you
  posted on someone else's PR carries no `class=` and never will — do not report its absence
  as a finding;
- every issue `board.sh` lists under "replied, but not by someone who may decide", and every
  candidate you read as *not* a decision — with the sentence you based that on, so a human can
  overrule you in one reply.

`queue` comes straight off the last line of `board.sh`. It is four integers and it is the
cheapest possible answer to "is this pipeline actually moving".

## Then render the human half

Under the JSON, at most **1200 characters**, in this order and nothing else:

    ## Needs you
    1. #721 — platform builds never compiled the new FFI path.
       `gh workflow run ci.yml --ref wagenet/685-hdrext -f platforms=both`

    ## Done
    | # | Verdict | Radius | Conf | CI |
    |---|---|---|---|---|
    | #721 | Comment | GLOBAL | HIGH | linux green, mac/win skipped |
    | #725 | Approve | SHARED | HIGH | green, new tests ran |

    Skipped: #726 #723 #700 (drafts), #698 #696 (dep bumps).
    Left by cap: #700.

Rules for the rendered half:

- If `needs_human` is empty, write `## Needs you` followed by `Nothing.` — one line.
- If `reviews`, `triage` and `fix` are all empty, **do not emit a table at all.** Write one
  sentence saying what you checked and why nothing needed work. A table of em-dashes is noise.
- Never a column that is not in the JSON. Never prose inside a table cell — if a radius needs
  three lines of explanation, that explanation belongs in the review, and the cell holds the
  token.

Finish by rewriting the `state:begin`/`state:end` block in the issue body.

## Reporting onward

If the task definition gives you somewhere else to report, render that message from
`needs_human` and the item lists — never from a second pass over the run. If the run did
nothing, say that in one line rather than staying silent.

**Every reference is a full URL, and every item is its own line.** A bare `#721` autolinks
only inside this repository's own issues and pull requests. Everywhere else it is four
characters a reader has to go look up by hand, so a message that names six items costs six
searches. Write `<https://github.com/Eyevinn/strom/issues/721|#721>` for a destination that
takes that link form, `https://github.com/Eyevinn/strom/issues/721` where it does not, and
`#721` only in the rendered half above, which is posted here. The `/issues/` path serves pull
requests too, so one form covers both and you never have to know which an item is.

One line per item, numbered under each heading. Do not merge two items into a sentence,
and do not write the message as prose — a paragraph naming four pull requests and their
verdicts is the shape this section exists to prevent.

    *Agent triage* — 2026-08-31T08:00Z, main@1c06c37, 4 items

    *Needs you*
    1. <https://github.com/Eyevinn/strom/issues/721|#721> — platform builds never compiled the new FFI path
       `gh workflow run ci.yml --ref wagenet/685-hdrext -f platforms=both`
    2. <https://github.com/Eyevinn/strom/issues/700|#700> — draft, needs `/agent-fix` to reach class A

    *Reviewed*
    1. <https://github.com/Eyevinn/strom/issues/721|#721> — Comment, GLOBAL, HIGH
    2. <https://github.com/Eyevinn/strom/issues/725|#725> — Approve, SHARED, HIGH

    Skipped 5, left by cap 1. Queue: 6 candidates, 1 claimed, 8 awaiting reply, 1 untriaged.

Cap the item lines at twelve. Past that, keep every `needs_human` line and collapse the rest
into the trailing count — the onward message is a prompt to act, and the full record is in
the summary comment.

Links into this repository are required, not merely allowed. What must never appear: a
credential or token, a service endpoint or instance hostname, a log excerpt, or a link to
anything outside this repository.
