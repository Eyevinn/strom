# Monthly dependency round

Read `PROTOCOL.md` first.

Dependabot opens security updates only. Keeping up with everything else is this round: once a
calendar month, one pull request that moves `Cargo.lock` forward and says what is still
behind.

## When

Before Phase 2 of `FIX.md`, look for this month's round (UTC):

    gh pr list --state all --search '"kind=deps month=2026-10" in:body' --json number,state

- **None, in any state:** do the round. It takes this run's one new PR, so skip Phase 2. If
  last month's round is still open, close it with a comment naming the new one.
- **Open:** follow it up like any other PR of yours in Phase 1 (below), then carry on as normal.
- **Merged or closed:** this month is done. A closed round is the maintainer's answer; do not
  open a second one.

## The round

Branch `agent/deps-<YYYY-MM>` from `origin/main`.

1. **Move the lockfile.** `cargo update --verbose` from the workspace root. This only takes
   semver-compatible versions. Keep the output: its `Updating` lines are what changed, its
   `Unchanged ... (available: ...)` lines are what is behind.
2. **Build and test as CI does.** Run the `cargo fmt`, `cargo clippy` and `cargo test` steps
   that `.github/workflows/ci.yml` runs on Linux. If you cannot build, say so once in the
   body and let CI be the check.
3. **A crate that breaks the build is held back, not worked around.** Find it from the error,
   pin it with `cargo update <crate> --precise <previous>`, and rebuild. Never change code to
   accommodate a compatible release. That is a bug upstream, and it goes in the body with the
   error line.
4. **Direct dependencies behind a breaking version.** For each crate in a workspace
   `Cargo.toml` whose newest version is semver-incompatible (a major bump, or a 0.x minor
   bump):
   - If raising the requirement builds and tests clean **with no code change**, raise it in
     **its own commit**, `chore(deps): bump <crate> <old> -> <new>`, so any one can be dropped.
   - Otherwise leave it, and list it under "Behind" with the first error or the reason.
   - **Never raise the `gstreamer*`, `gst-plugin*`, `glib*`, `egui*`, `eframe` or `wgpu`
     requirements here.** They move together and each release needs reading. List them under
     "Behind" with the version available and a link to the release notes.

The lockfile update is commit 1. Each requirement bump is one commit after it.

## The PR

Title `chore(deps): monthly dependency round <YYYY-MM>`. Open it ready for review. Body target
1500 characters, ceiling 3500 (`verify-citations.sh --allow-no-citations --max-chars 3500`):

    Monthly dependency round, 2026-10. Lockfile moved on 23 crates; 2 requirements raised.

    **Updated** (notable only: direct dependencies and anything GStreamer):
    tokio 1.52.3 -> 1.53.1, uuid 1.23.4 -> 1.26.1, gstreamer 0.25.3 -> 0.25.5

    **Raised** (one commit each, droppable):
    - base64 0.22 -> 0.23: builds and tests clean, no code change

    **Held back:**
    - gstreamer-video 0.25.6: calls a gstreamer-video-sys symbol its declared floor lacks
      (`error[E0425]: cannot find function ...`). Pinned at 0.25.5.

    **Behind** (not taken):
    - egui 0.35 -> 0.36: family bump, needs reading. <release notes link>
    - nvml-wrapper 0.12 -> 0.13: 4 errors, `Device::memory_info` signature changed

    **Verified:** cargo clippy and cargo test (strom, strom-types, strom-frontend) pass
    locally at the head commit. WASM clippy is left to CI.

    <!-- strom-agent protocol=v3 kind=deps month=2026-10 pr=812 -->

"Behind" is the part a maintainer acts on. One line per crate, with the reason it was not
taken. Never an unexplained list.

## Follow-up in Phase 1

An open round of yours is followed up every run until it is merged or closed:

- **CI red because of a crate:** hold that crate back as in step 3, or drop its bump commit,
  force-push, and update the body.
- **Conflicts with `main`:** recreate the branch from `origin/main` and redo the round. A
  lockfile is never merged by hand.
- **Older than 14 days with no human comment:** leave it open. `FIX.md`'s stale-PR closure
  does not apply; next month's round replaces it.

## Reporting

In the run summary, record the round under `fix` as
`{"pr": 812, "kind": "deps", "month": "2026-10", "ci": "..."}`. Put anything under "Behind"
that is a security or end-of-life question in `needs_human`.
