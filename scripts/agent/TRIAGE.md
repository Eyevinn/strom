# Triaging an issue

The goal is not to judge the issue. It is to place it: what it is really about, what it
belongs with, and where someone picking it up should start reading. Whoever picks it up will
investigate again with fresh eyes. Give them leads, not conclusions.

## Look around before you write

1. **Read the issue and its comments.**
2. **Find what it belongs with.** Start from the open board you read at the start of the run
   (`ROLE_REVIEW.md`), then search closed issues and merged pull requests too, for the
   subsystem, the block, the element, the symptom:

       gh issue list --state all --search "<term>" --limit 30
       gh pr list --state all --search "<term>" --limit 30
       git log --oneline -20 -- <path>

   An item is related when it shares a cause, shares a design decision, changes the same code
   path, or would make this one unnecessary. Mentioning the same word is not related.
   Recently merged work in the same area counts: an issue that appears right after a change
   is often caused by it.
3. **Read enough code to know which part of Strom this lives in.** You need the area, not the
   line. If the issue states something about how the code behaves, check it.
4. **Search the web** when it helps: an upstream GStreamer issue or merge request, an element's
   documentation, a spec, a known limitation, how another project solved the same thing.
   Link what you used. If web search is not available in this run, skip this step.

## What to write

One comment, at most **1000 characters**, in this shape. Leave out any section with nothing
in it.

    **Area:** a few words naming the part of Strom and the concern, e.g. "WHIP ingest,
    session lifecycle". Free text, not a category from a list.

    **About:** one or two sentences on what the problem actually is. If the reported symptom
    and the underlying problem differ, say both.

    **Relates to:** #N (shares the cause), #M (same code path), merged #K (likely introduced
    it). This is the most important line: say how each one relates.

    **References:** upstream issues, docs, specs, with links.

    **Thoughts:** a sentence or three. Is this a symptom of something larger? Is it already
    possible some other way? What direction looks right, and what looks like a trap?

    <!-- strom-agent triage -->

Rules:

- No code, no line numbers, no quoted source. Name a module or a file if it helps.
- No questions to the maintainer, no options to choose between, no verdicts.
- If you could not tell something, say that in a few words instead of guessing.
- Write for the person who reported it as well as for the maintainer: plain sentences.
- If the issue is already solved or already possible, say how, briefly, with a link to the
  commit or PR.
- If the report lacks what anyone would need to reproduce it, say what is missing in one
  sentence.

## Updating

When something changes (see `ROLE_REVIEW.md`), **edit your comment** rather than adding one:

    gh api -X PATCH repos/Eyevinn/strom/issues/comments/<comment_id> -F body=@body.md

Rewrite it as it should read now; do not append "Update:" paragraphs. Writing `#N` in the
comment makes GitHub show the link on #N too, so relations show up on both issues.

## Old-format comments

Many issues still carry long triage comments from an earlier protocol (they contain
`protocol=v3`). Do not add a new comment beside them. Edit the **most recent** one into the
format above, and minimize the older ones as outdated so the thread reads short:

    gh api graphql -f query='mutation { minimizeComment(input: {subjectId: "<node_id>", classifier: OUTDATED}) { clientMutationId } }'

Only ever edit or minimize comments that are yours by the marker.
