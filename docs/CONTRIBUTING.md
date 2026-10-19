# Contributing to Strom

Thank you for your interest in contributing to Strom! This document explains how to contribute.

## How this project is built

Strom is written by **Claude Code**. The codebase is authored by AI — humans are not meant to hand-write the code. People set direction, review, and steer; the AI writes the implementation. This is by design, and it shapes how to contribute.

## Start with an issue

**We would rather have a good issue than a pull request.** Code is quick to generate and goes stale quickly: Strom changes fast, and a pull request that is not merged within days needs rebasing, and often rethinking. Most problems also turn out to be connected to other work in progress, which is easier to see from an issue than from a diff.

A useful [issue](https://github.com/Eyevinn/strom/issues) says:

- **What is broken**, or what is missing, and how to see it: steps, a flow export, logs.
- **How you would solve it**, if you have an idea. A direction in a few sentences is worth more than a diff.
- **A test case**: how we would know it works. A reproduction we can turn into a test is the most valuable thing you can give us.

Ideas and open questions also fit in [GitHub Discussions](https://github.com/Eyevinn/strom/discussions).

Pull requests are still welcome, especially small ones tied to an issue, but expect us to sometimes take the idea and implement it differently alongside related work.

Because the code is AI-authored and evolves quickly, **the code is the source of truth**, not the documentation. Docs in this repo are for navigation and high-level understanding (what Strom is, what it can do, how to set it up, how things fit together). Documents that describe internal design or implementation live in [`archive/`](archive/) with a disclaimer — assume they have drifted, and read the code for current behaviour.

## Development setup

See [DEVELOPMENT.md](DEVELOPMENT.md) for prerequisites, building, running, and the full toolchain. A working local build is useful both for testing and for steering an agent.

After cloning, install the Git hooks so formatting and linting run automatically before each commit:

```bash
./scripts/install-hooks.sh
```

## Code quality standards

All changes must pass these before being merged. CI enforces all three; the pre-commit hook runs
formatting and clippy, but not the tests:

```bash
cargo fmt --all                                       # formatting
cargo clippy --workspace --all-targets -- -D warnings  # linting, no warnings
cargo test --workspace                                 # tests
```

CI runs the same checks on every pull request — format, clippy, tests, and a full frontend + backend build — plus Docker image builds on release.

## Making changes

1. Branch from `main`:
   ```bash
   git checkout -b feature/your-feature-name
   ```
2. Make the change and confirm the checks above pass.
3. Commit (hooks run automatically) and push to your fork.
4. Open a pull request.

### Block guidelines

Blocks follow one contract: a block emits what it naturally produces, and each consuming block adapts its own input. Output blocks take encoded video and either raw or encoded audio. Read [BLOCK_GUIDELINES.md](BLOCK_GUIDELINES.md) before adding or changing a block.

### Pull request guidelines

- Open or link an issue first, so the problem is discussed separately from one way of fixing it.
- Provide a clear description of *what* changed and *why*.
- Keep changes focused and atomic; add tests for new behaviour.
- Update docs only where they are navigational — don't add code-describing docs (see [How this project is built](#how-this-project-is-built)).
- Ensure all CI checks pass.

### Draft pull requests

A draft is fine for as long as someone is working on it. When it goes quiet, it gets asked about:

- **After 2 weeks without activity**, the draft gets a comment asking whether it should stay open. Activity means new work: commits written in that time, or discussion on the PR. A rebase does not count, since it only moves the same commits forward.
- **Any reply keeps it open.** If no one replies within another week, it is closed.
- **Closing is not deleting.** The branch is kept, and the PR can be reopened at any time.

## Review

An AI agent comments on new issues with related work and references, and reviews pull requests that are not drafts. Its reviews are input, not a verdict: a maintainer decides what is merged.

## License

By contributing to Strom, you agree that your contributions will be licensed under the same license as the project (MIT OR Apache-2.0).
