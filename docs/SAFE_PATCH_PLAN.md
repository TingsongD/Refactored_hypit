# Safe patch plan

Base: `3fa7f5aa5e2bc7bfb256048c109949ac4b6e3f33` (main, 2026-09-20).

## Scope and invariants

1. Preserve existing render outputs and capability assets on every failure.
   Share a staging helper backed by an exclusively created private temporary
   directory on the destination filesystem. Publish with one rename; never
   delete the destination to make a retry possible. Clean staging after child
   processes have closed their files.
2. Keep the subprocess deadline in effect until the child and both output
   readers are finished. Feed request bytes through an anonymous temporary
   file so an unread stdin cannot block a writer thread. Retain the process group / Windows Job
   Object after the parent exits, so descendants cannot hold pipes indefinitely.
   Route capability calls through the same bounded capture implementation.
3. Arm media stall detection only while a pipe read/write is outstanding.
   Idle render workers and script warm-up must not count as stalled I/O.
4. Import external footage for every explicit `adapt --out`, including a bare
   filename (whose parent is the current directory).
5. Fix Clippy failures under the CI toolchain without weakening the lint gate.

## Regression gates

- Existing output survives missing/empty staged output and failed publication;
  successful publication replaces it; staging is private and collision safe.
- Capability spawn failures, nonzero exit, no output and empty output preserve
  the previous asset; a valid nonempty output replaces it.
- Parent-exits-first descendants holding stdout/stderr and a child that never
  reads a large stdin time out; normal output and nonzero status are preserved.
- Inactive and completed I/O survives past the stall interval; an outstanding
  operation still terminates a hung child.
- CLI adaptation with `--out draft.scene` imports outside footage and emits a
  confined source.
- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo test --workspace`, and `SCENE_MEDIA_TESTS=1 cargo test --workspace`.
- Open a PR against main and inspect Linux/macOS/Windows CI; do not merge.

## Boundaries

No format, timing, rendering or connector API expansion. Connector requests
still receive an `out` path, but it is a staging path; only the engine publishes
the final asset. Atomic replacement does not promise power-loss durability.
Subprocess connectors must keep descendants in the managed process group/job;
deliberately daemonizing executables are outside the connector contract.

## Rollback

Keep the changes on a dedicated branch. Revert the PR as a unit if necessary;
there is no persisted schema migration or user-data conversion.
