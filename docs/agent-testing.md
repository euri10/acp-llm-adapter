# Testing and acceptance

Required by [AGENTS.md](../AGENTS.md) before executable changes, test or
reliability design, gate changes, or CI work. Read once before applicable work.

## The loop

Meaningful behaviour changes use red-green-refactor:

1. Write the failing test first and **run it**, confirming it fails for the
   reason you expect. A test that passes before the fix is not evidence.
2. Make the minimal change that turns it green.
3. Refactor with the suite green.
4. Run the full validation below before handoff.

Pure refactors start from passing characterization coverage — if the behaviour
is not pinned by a test, pin it before moving the code. Documentation,
formatting, and trivial mechanical edits need no artificial red test; say so
explicitly when you skip it.

## Full validation procedure

Run in this order. Establish the baseline **before** touching any code, so a
pre-existing failure is never mistaken for one you introduced:

```sh
# 1. Baseline, before your changes.
git stash
cargo test -q 2>&1 | grep -E "^test result|FAILED"
git stash pop

# 2. The same suite with your changes applied.
cargo test -q 2>&1 | grep -E "^test result|FAILED"

# 3. Formatting and linting.
cargo fmt --all && cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings

# 4. Public documentation.
cargo doc --no-deps --all-features   # RUSTDOCFLAGS="-D warnings" in CI
```

Every `test result` line must say `ok`. Any `FAILED` line that was not in the
baseline must be fixed before the task closes. Never stash the maintainer's
uncommitted work without saying so — if `git status --short` shows changes you
did not make, take the baseline from CI or a clean worktree instead.

The two harness-less integration fixtures (`mcp_stdio_fixture`,
`acp_proxy_fixture`) spawn real processes. They must pass too; a hang there is a
lifecycle defect, not a flake to retry.

## Gates

CI (`.github/workflows/ci.yml`) enforces fmt, clippy, `cargo test --all-targets
--all-features`, doc-tests, `cargo doc` with `-D warnings`, a release build,
`cargo audit`, and the AGENTS.md size check. Tests run with
`LLM_API_KEY=skip-ci-no-key` so an accidental network call fails fast rather
than hanging.

A test must never depend on a live provider endpoint. Use the mock `LlmClient`
or a local fixture server; if a test cannot be written without the network, it
is the wrong test.

Report unavailable gates rather than skipping them silently. Report pre-existing
failures in unrelated in-flight files; do not fix another agent's work.

## Conventions

New tests that benefit from tracing output use `test-log`, declared in
`[workspace.dependencies]` and picked up via `[dev-dependencies] test-log = { workspace = true }`.
Only the `trace` feature is enabled, so events flow through `tracing-subscriber`,
not `env_logger`.

```rust
#[test_log::test]
fn synchronous_test() { /* tracing events become visible on failure */ }

#[test_log::test(tokio::test)]
async fn async_test() { /* same, for async */ }
```

Use `tracing-test` only per-test, when a test must assert on log output — for
example a silent recovery path with no other observable side effect. Do not
blanket-add either crate to existing tests; adopt them as new tests are written.

- Property-based tests (`proptest`) go in `#[cfg(test)]` modules alongside the
  unit tests they cover.
- Benchmarks (`criterion`) live in `benches/`.
- Coverage uses `cargo llvm-cov`. Do not use `cargo tarpaulin`.

## What coverage must not lose

Preserve protocol, security, lifecycle, async, and public-behaviour coverage.
Specifically:

- ACP request/response shapes and JSON-RPC framing, including malformed and
  unknown-field cases.
- Provider SSE parsing: partial frames, truncated streams, error payloads.
- Permission gating and path confinement on tool execution — the blast radius.
- Session lifecycle: creation, teardown, client disconnect, and the absence of
  orphaned processes or tasks afterwards.
- Error mapping at the translation boundary, including that internal detail is
  sanitized before it reaches the editor.

Removing a test requires a recorded replacement contract. Churn in a file is not
evidence that its old contract is obsolete.

## Acceptance

For a live defect, state the acceptance criterion before implementing, and keep
the issue open until the maintainer confirms the exact reproduction is gone.
Automated green gates alone do not close a defect that was reported from real
use. A feature behind optional configuration needs an unset-case test as well as
the configured one.
