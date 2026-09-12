# Rust policy

Required by [AGENTS.md](../AGENTS.md) for Rust code, manifests, lints, unsafe,
API documentation, design, or review. Read once before applicable work,
together with [testing](agent-testing.md).

## Toolchain and manifests

- Stable toolchain only, matching `RUST_VERSION` in CI and `rust-version` in
  `Cargo.toml` (currently 1.95, edition 2024). Default rustfmt; the edition stays
  explicit in the manifest.
- Declare an MSRV only when it is actually tested. Do not claim compatibility
  from an untested `rust-version` field.
- Dependencies are pinned deliberately and updated by Renovate. A major bump on
  a protocol crate (`agent-client-protocol`, `rmcp`) is a deliberate change with
  a full regression pass, not a routine update.

## Lints

Lints are configured once in the root `Cargo.toml` under `[workspace.lints]` and
picked up by the package with `lints.workspace = true`, so binaries, library and
tests are all covered. The crate root additionally carries `#![forbid(unsafe_code)]`.

Denied: `warnings`, `missing_docs`, `unreachable_pub`,
`missing_debug_implementations`, `clippy::all` and `clippy::pedantic` (group
priority `-1`), `clippy::unwrap_used`, `clippy::expect_used`, `clippy::panic`,
`clippy::todo`, `clippy::unimplemented`, `clippy::indexing_slicing`,
`clippy::get_unwrap`, `clippy::unwrap_in_result`, `clippy::print_stdout`,
`clippy::dbg_macro`, `clippy::exit`.

- Fix lint findings. Never disable `warnings`, `all`, or `pedantic`, and never
  add a crate- or module-wide production suppression.
- A narrow exception must name the lint and explain why the code is correct and
  clearer as written. Prefer `#[expect(..., reason = "...")]` when the lint is
  expected to fire; use a justified `#[allow]` only when an expectation is
  inappropriate. "Clippy is noisy" is not a justification.
- Never suppress `clippy::missing_errors_doc` or `clippy::missing_panics_doc`.
  Fix the underlying issue by adding the `# Errors` / `# Panics` section.
- Do not raise a threshold to avoid fixing code. Split mixed responsibilities
  rather than creating an artificial helper to satisfy a length lint; a cohesive
  function may take a narrow, justified exception.
- Existing allows in this tree that are legitimate and must keep their comment:
  `must_use_candidate` (blanket `#[must_use]` is noise at this stage), and the
  test-module blanket on `indexing_slicing` (test assertions deliberately panic).

## Unsafe

`unsafe_code` is forbidden crate-wide. Platform operations that would otherwise
need it go through a safe wrapper crate — this is why `rustix` is a Linux-only
dependency for descendant process control rather than local syscall wrappers.

Before adding or expanding any unsafe boundary, record why safe stdlib or
existing-dependency APIs do not suffice, the alternatives considered, and the
evidence supporting the chosen boundary. Existing unsafe code receives no
automatic exemption.

If an exception is ever granted, each unsafe operation needs a precise
`// SAFETY:` argument covering its actual obligations (validity, alignment,
aliasing, descriptor ownership, post-fork restrictions). Keep unsafe blocks
minimal and encapsulated behind a safe API that enforces its invariants. A green
test does not establish soundness.

Also prohibited without formal justification: `std::mem::transmute`,
`mem::zeroed`, `mem::uninitialized`, raw pointer arithmetic, manual `Drop`
manipulation, self-referential structs and incorrect `Pin` usage, `static mut`,
manual `Send`/`Sync` impls, FFI without a safe wrapper, and artificial lifetime
extension.

## Failure handling

- Expected input, I/O, configuration, and protocol failures return typed
  `Result` errors (`thiserror` domain errors). Preserve their causes and
  sanitize them at the presentation boundary.
- Reserve panics for violated internal invariants that cannot be expressed in
  the type system, and document them. There are none in library paths today;
  keep it that way.
- Never swallow an error silently. A discarded `Result` is a bug unless the
  discard is explicit and commented.
- Prefer `slice.get(i)` over `slice[i]`. Avoid invalid UTF-8 assumptions on
  provider bytes.

## Ownership and API design

- Accept `&str` not `&String`, `&[T]` not `&Vec<T>`. Avoid unnecessary `.clone()`;
  when a clone is the simple correct answer, keep it and say why.
- Public APIs are documented — `missing_docs` is denied and `cargo doc --no-deps`
  runs with `-D warnings` in CI. Every public fallible function has `# Errors`;
  every public panicking function has `# Panics`.
- Keep ACP wire types and HTTP/SSE types out of the harness core. The
  translation boundary at the edges is what keeps the agent loop testable.
- Extension happens at the established boundaries — the `LlmClient` trait, the
  tool registry, the session store, CLI backend selection. Do not add new traits
  or generics for a single implementation.

## Concurrency

- One runtime (tokio) throughout. Never mix async runtimes.
- Never hold a lock across `.await`, and never block inside async. Use
  `spawn_blocking` or the `blocking` crate for genuinely blocking work.
- Define lock ordering where more than one lock is live. Avoid nested
  `Arc<Mutex<T>>`.
- No hidden global mutable state: `Arc`, `Mutex`, `RwLock`, `OnceLock`, or
  dependency injection.
- Spawned children and tasks have an owner responsible for cancelling and
  reaping them. A dropped session must not leave an orphaned process or task.
