# Documentation

## User documentation

- [Configuration](configuration.md) — environment variables, `--backend`, reasoning effort, backend defaults.
- [Sessions and modes](sessions.md) — system instruction, reasoning replay, history budget, modes, settings persistence, `/clear`.
- [Tools and permissions](tools-and-permissions.md) — built-in tools, file-root confinement, cancellation, `run_command`, MCP tools.
- [Selected-content sessions](selected-content.md) — the tool-less, one-attempt helper session contract.
- [Logging and debugging](logging.md) — structured logs, `acp-proxy`, retention, redaction, tracing.
- [Usage and cost](usage-and-cost.md) — `usage_update` cost, `LLM_PRICING`, usage validation, context-window reporting.
- [ACP protocol coverage](acp-coverage.md) — supported ACP methods and load/resume/close semantics.
- [Mock backend](mock-backend.md) — offline `--backend mock`, including the `!tool` affordance.
- [Architecture](architecture.md) — channel diagram, module map, transport limits, design principles.
- [Library API](library.md) — the reusable `llm` module and a streaming example.

## Agent contract

- [Agent workflow](agent-workflow.md) — Beads tracker mutation, claims, and session attribution.
- [Rust policy](agent-rust.md) — Rust code, manifests, lints, API docs, design, and review.
- [Testing and acceptance](agent-testing.md) — test and reliability design, gates, and CI.
