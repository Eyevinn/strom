# Agent protocol

One scheduled agent triages Strom's issues and reviews its pull requests. These files are
what it follows. A pull request against them is a pull request against how the agent behaves.

| File | Read when |
|---|---|
| `ROLE_REVIEW.md` | Every run, first. Role, order, budget, reporting. |
| `TRIAGE.md` | Triaging an issue. |
| `REVIEW.md` | Reviewing a pull request. |

The task definition outside this repo holds the rest: credentials, schedule, where it
reports, and what it is allowed to do. Nothing here names infrastructure, so it is safe in a
public repo, and nothing here can widen the permissions, because a pull request can reach
these files.

## Why it looks like this

An earlier version ran a second agent that turned issues into pull requests, and asked the
maintainer for a design decision on every issue it triaged. It produced long comments nobody
read and pull requests that could not be checked without running Strom, which the agent
cannot do. Strom is too interconnected to work one issue at a time. So the agent now places
an issue among the work around it and points to further reading, and the maintainer picks up
an area with Claude Code locally, where it can be built and run.

Two things from the earlier version are deliberate and should stay:

- **The agent reads these files from `origin/main`**, never from the working tree. Reviewing
  a pull request checks out someone else's branch, and a branch can edit these files.
- **A comment is the agent's by its marker, never by its author.** The agent may post from a
  maintainer's own account, and keying on the login throws away the maintainer's comments.
