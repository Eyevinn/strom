Closes #
<!-- or "Refs #" if this PR only partly addresses the issue: "Closes" closes it on merge. -->

## Problem

<!-- What is wrong or missing, and why it matters. -->

## Change

<!-- What you did and why this approach. Name what you rejected. The repo keeps no design
docs, so the PR is the design record.
A new block: name the existing block closest to it, and why this is not a mode of it. -->

## Evidence

<!-- One line per behavioural claim, then what shows it:
- a test that fails when the change is reverted: name it, and quote the failure;
- a rig or manual run: what ran, on what, and what you saw;
- "unverified", and why.
Capture exit codes: `gst-launch-1.0 ... | tail` reports success after a segfault.
A test that passes both before and after guards a rewrite, not the claim. Say which. -->

## Tests

<!-- Ran: the commands, the platform, the result.
Skipped: tests that returned early (missing element, no GL context, no device).
Not run: anything relevant you did not run, and why.
"CI is green" does not show that the new test executed. -->

## Lesson

<!-- Did this bug, review or design teach something future work needs? Link the rule
added or changed in CLAUDE.md or docs/BLOCK_GUIDELINES.md, or the test that now guards
it. "None" is a valid answer. -->

## For the reviewer

<!-- Decisions a maintainer has to make, risks, and what else changes behaviour
(other blocks, platforms, API or WebSocket contract). Delete if none. -->

## Out of scope

<!-- Known limits this PR leaves alone. Review findings on this PR are fixed here; a
follow-up is a linked issue with the reason it is out of scope. Delete if none. -->
