# acp-llm-adapter Development Contract

## Project Scope

acp-llm-adapter is an early-stage Rust binary with no external users. It
presents LLM providers (DeepSeek, GLM) as coding agents to ACP-capable editors:
JSON-RPC over stdio towards the editor, HTTPS + SSE towards the provider, and
the agent harness — system prompt, tool catalogue, the prompt→tool-call→execute
→feed-back loop, history, permission gating — in between.

Breaking changes are acceptable. Change the code directly; do not add
compatibility shims, deprecated aliases, or migration paths unless explicitly
requested.

Priority: **correctness (including safety and security) → clarity → simplicity
→ performance**.

Fix root causes at the narrowest shared boundary. Prefer deletion, existing
code, the standard library, and plain functions over new abstractions. Do not
build extension points for hypothetical consumers. The established extension
boundaries are the `LlmClient` trait, the tool registry, the session store, and
the CLI backend selection.

## Required references

This file is the always-loaded contract. The following repository documents are
also binding; read the applicable document before that work, once per session.
A complete, current copy already in context needs no second read. Recover only
missing/truncated sections, and re-read when the file changes.

| Before doing this | Read |
| --- | --- |
| Any tracker mutation, claim decision, or session-attribution question | [Agent workflow](docs/agent-workflow.md) |
| Rust code, manifests, lints, unsafe, API docs, design, or review | [Rust policy](docs/agent-rust.md) |
| Executable changes, test or reliability design, gates, or CI | [Testing and acceptance](docs/agent-testing.md) |

The root file is limited to 12,000 bytes by `./scripts/check-agent-instructions`,
enforced in CI. Put detailed procedures in the relevant required reference;
preserve the rule and its routing trigger here. This leaves room below Codex's
32-KiB project-instruction cap.

## Work tracking and triage

Beads (`br`) is the shared record; `.beads/` is committed and its JSONL export
is what crosses sessions. Read the graph through `br`/`bvr`, never by parsing
`.beads/*.jsonl`; never initialize Beads implicitly. Durable context belongs in
the repository: issue-local evidence in Beads, standing rules here or in a
required reference, never private agent memory.

- Recommendation requests are read-only: report issue IDs, priority, and one-line
  reasoning, then stop. Claim, implement, or commit only when asked to proceed.
- Run `br robot-docs guide` for installed syntax. Use only `bvr --robot-*`;
  bare `bvr` blocks in a TUI. `bvr` is beads_viewer_rust — this project does not
  use the older Go `bv`.
- Every mutating `br` command takes `--actor "<current session id>"`, not just
  claims. Claim with `br update <id> --claim --actor "<id>"`, never a bare status
  change. Finish your own claimed work before taking new work; another agent's
  live claim is protected. See the workflow reference before mutating; if
  attribution fails, ask.
- File defects before fixing them, preserving the failing evidence. Record
  unfinished or deferred work as issues before handoff. Use dependencies for
  actual blockers.
- Search prior lessons with `br search -a`; closed issues are otherwise omitted.
  `br list` does not return comments: use `br show`/`br comments list`.
- Before closing a bug/task/feature, route the lesson to its issue, a required
  instruction, or a `needs-design` question, or explicitly say none is warranted.

### Bounded query recipes

Filter at the CLI and project fields before output reaches the tool response.
Do not stream complete scheduler/ready payloads or parse tool-truncated JSON.
Limit the combined output of parallel calls too: individual tool budgets do not
prevent the outer response being truncated.

For a next-task recommendation, get a small ranked shortlist:

```sh
br scheduler --limit 5 --json | jq '[.recommendations[] |
  {id: .issue.id, title: .issue.title, priority: .issue.priority,
   labels: .issue.labels, score, rationale}]'
```

The scheduler ranks ready work and excludes claimed issues. For scoped work
discovery use `br ready`, which applies project readiness policy:

```sh
br ready --limit 5 --json | jq '[.[] | {id,title,priority,labels}]'
```

Fetch full `br show <id> --json` only for candidates needing detail. Widen the
limit only when the shortlist cannot answer the request; state that a bounded
list is a shortlist, not an exhaustive inventory. Graph-specific questions may
use `bvr --robot-next` or a projected `bvr --robot-triage` result; add
`--format toon` to cut context cost. Inspect the installed schema/help when a
shape is unknown; do not discover it with a raw backlog dump. These recipes were
checked with br 0.5.12 and bvr 0.3.0.

### Project vocabulary

Canonical meanings live only in Beads `vocabulary/reference` records. Before
semantic work (planning, implementation, review or issue filing), load this
compact index once, then read the full definitions of applicable terms:

```sh
br list --type vocabulary --status all --json | jq '[.issues[] |
  select(.issue_type == "vocabulary" and .status == "reference") |
  {id,title,description}]'
br show <applicable-term-id> --json | jq '[.[] | {id,title,description,notes,design}]'
```

The all-status query intentionally returns an empty result if there is no
vocabulary yet. Pure housekeeping may skip it. Do not scan for missing terms or
treat absence as debt. Do not duplicate definitions into instructions. Material
conflicts involving vocabulary, code, issues or user intent require a
`needs-design` question and shared understanding with the user; no source wins
automatically. Never mutate vocabulary silently or promote before the user's
confirmation.

## Architecture and lifecycle

- Business logic must not depend on I/O. Keep ACP wire types and HTTP/SSE types
  out of the harness core; the translation boundary is what makes the loop
  testable without a socket or a provider.
- A provider is reached only through `LlmClient`. Tests and `dev` use the mock
  implementation, not a live endpoint.
- `mod` declarations define and return APIs. Importing a module must not spawn
  processes, open sockets, or mutate global state.
- Mutable state has a clear owner and lifecycle, never a hidden module
  singleton; sessions live in the explicit session store. No `static mut`; use
  `Arc`, `Mutex`, `RwLock`, `OnceLock`, or dependency injection.
- Spawned children and tasks have an owner responsible for cancelling and
  reaping them. Repeated session setup/teardown must not leak processes,
  descriptors, or tasks.
- One async runtime (tokio) throughout. Never hold a lock across `.await` and
  never block inside async.
- stdout is the JSON-RPC wire. Nothing but protocol frames goes there —
  diagnostics go to stderr through `tracing`.
- Avoid circular module dependencies. Split modules when responsibilities
  diverge, not in anticipation of future growth.

## Errors and validation

- Expected input, I/O, configuration, and protocol failures return typed
  `Result` errors. No `unwrap`, `expect`, `panic!`, `todo!`, or
  `unimplemented!` in production paths.
- Never swallow an error silently. Preserve the cause internally; sanitize at
  the presentation boundary so internal detail does not reach the editor or a
  provider.
- Validate all external input — editor requests, provider responses, tool
  arguments, configuration — before changing state.
- For ACP/JSON-RPC and provider SSE, validate the fields you consume and reject
  malformed or contradictory messages, but ignore unknown optional fields from
  newer peers.
- Prefer `slice.get(i)` over `slice[i]`; `indexing_slicing` is denied outside
  tests.

## Security

- Never log API keys, tokens, environments, prompts, or tool payloads by
  default. Use constant-time comparison for secrets.
- Tool execution is the blast radius: permission gating and path confinement are
  correctness requirements, not conveniences.
- Keep the dependency tree minimal and prefer well-maintained crates with no
  duplicate functionality. A new dependency requires a demonstrated gap; run
  `cargo audit` before adding one. Never vendor a dependency for convenience.

## Editing and completion

Preserve unrelated user/agent changes, make the smallest root-cause change, and
revise existing files instead of creating versioned copies (`handler_v2.rs`,
`session_old.rs`). New files are only for genuinely new functionality. Never
stash user work or run destructive Git/filesystem commands — `git reset --hard`,
`git clean -fd`, `rm -rf` — without explicit instruction; if uncertain, stop and
ask.

- Meaningful behaviour changes use red-green-refactor. Pure refactors start from
  passing characterization coverage. Documentation, formatting, and trivial
  mechanical edits need no artificial red test; explain exceptions.
- Never rewrite code files with `sed`/regex pipelines. Use `ast-grep` when
  structure matters and `ripgrep` when text is enough; combine them by
  shortlisting with `rg -l` and matching with `ast-grep`.
- Look up a third-party crate's current documentation rather than guessing at
  method signatures or feature flags.
- Provider constants — prices, context windows, model ids — are fetched from
  the provider's page or API when written, never recalled, and cite that source
  in a comment beside them. A wrong method signature fails to compile; a wrong
  price is a plausible number that a test written from the same memory will
  happily confirm (daa-groq-backend-jbm5).
- Run the applicable focused checks and the complete affected suite before
  handoff, following [testing](docs/agent-testing.md). Report unavailable gates
  and pre-existing failures in unrelated in-flight files; do not fix another
  agent's work.
- Update public documentation in the same change. Every public item is
  documented (`missing_docs` is denied) and every fallible or panicking public
  function carries `# Errors` / `# Panics`.
- Before an authorized commit, run `br sync --flush-only`. Inspect
  `git status --short`, account for unrelated paths, and stage only named paths
  you changed, including the relevant Beads records. Never `git add .` or
  `git add -A`.

If it compiles, it is not necessarily correct. If it passes tests, it is not
necessarily safe. If it is correct but needlessly complicated, simplify before
handoff.
