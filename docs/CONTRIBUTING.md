# Contributing to Strom

Thank you for your interest in contributing to Strom! This document explains how to contribute.

## How this project is built

Strom is written by **Claude Code**. The codebase is authored by AI — humans are not meant to hand-write the code. People set direction, review, and steer; the AI writes the implementation. This is by design, and it shapes how to contribute. We welcome feature requests, ideas, and pull requests:

- **AI-written changes.** Open a pull request whose changes were written by an AI coding agent (Claude Code or similar). Describe *what* you want and *why*; let the agent produce the diff.
- **Or just the idea.** Open a [GitHub Discussion](https://github.com/Eyevinn/strom/discussions) or file a feature request in [Issues](https://github.com/Eyevinn/strom/issues) — you don't have to write code at all.

Because the code is AI-authored and evolves quickly, **the code is the source of truth**, not the documentation. Docs in this repo are for navigation and high-level understanding (what Strom is, what it can do, how to set it up, how things fit together). Documents that describe internal design or implementation live in [`archive/`](archive/) with a disclaimer — assume they have drifted, and read the code for current behaviour.

## Development setup

See [DEVELOPMENT.md](DEVELOPMENT.md) for prerequisites, building, running, and the full toolchain. A working local build is useful both for testing and for steering an agent.

After cloning, install the Git hooks so formatting and linting run automatically before each commit:

```bash
./scripts/install-hooks.sh
```

## Code quality standards

All changes must pass these before being merged (the pre-commit hook and CI enforce them):

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

### Pull request guidelines

- Provide a clear description of *what* changed and *why*.
- Reference any related issue or discussion.
- Keep changes focused and atomic; add tests for new behaviour.
- Update docs only where they are navigational — don't add code-describing docs (see [How this project is built](#how-this-project-is-built)).
- Ensure all CI checks pass.

## Review

Reviews are handled by our Claude Code with human oversight. Address the feedback it raises; once the change is sound and CI is green, it can be merged.

## License

By contributing to Strom, you agree that your contributions will be licensed under the same license as the project (MIT OR Apache-2.0).
