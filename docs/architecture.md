# Architecture

The adapter bridges two independent channels:

```
┌────────────────────────────────────────────────────────────────────────────────────────┐
│                         acp-llm-adapter                                           │
│                                                                                        │
│  Editor ──ACP/stdio──▶ ┌─────────────────┐  ┌─────────────────┐                        │
│  (Zed,      JSON-RPC   │  acp.rs         │  │  llm/*          │                        │
│   Neovim,   frames  ◀──│  ACP transport  │  │  HTTPS + SSE    │──▶ LLM Provider API    │
│   ...)                 │  + request      │  │  client, types, │  │ (DeepSeek/GLM/Groq)  │
│                        │  handlers       │  │  stream parser  │  │ /chat/completions    │
│                        └─────────┬───────┘  └────────┬────────┘                        │
│                                  │                   │                                 │
│                           ┌──────▼───────────────────▼──────────────────┐              │
│                           │ · turn.rs           · tools.rs     · mcp.rs │              │
│                           │ · Session state     · tool loop    · MCP    │              │
│                           │ · Permission gating · cancellation          │              │
│                           └───────────────────┬─────────────────────────┘              │
│                                               │                                        │
│                                   ┌───────────▼───────────┐                            │
│                                   │   session_store.rs    │                            │
│                                   │   JSONL persistence   │                            │
│                                   └───────────────────────┘                            │
└────────────────────────────────────────────────────────────────────────────────────────┘
```

**Left side** — the adapter speaks the [Agent Client Protocol](https://agentclientprotocol.com) (ACP) over stdio as JSON-RPC 2.0 frames. The `agent-client-protocol` 3.x SDK handles the stable ACP v1 wire protocol; [`acp/`](../src/acp/) registers request handlers and translates between ACP schema types and the adapter's internal types. Its `process` feature supports the `dev` command's child agent, while the server uses the generic `Lines` transport.

**Right side** — the adapter speaks HTTPS + Server-Sent Events to the provider's OpenAI-compatible `/chat/completions` endpoint via a thin client owned by this crate in [`src/llm/`](../src/llm/). A [`LlmClient`](../src/llm/client.rs) trait provides the mock seam for testing without a live API key.

**Middle** — the adapter is the translator _and_ the agent harness. [`turn.rs`](../src/turn.rs) orchestrates the prompt→tool-call→execute→feed-back loop. [`tools/`](../src/tools/) registers built-in tools (read/write/edit files, glob, grep, shell commands) and routes execution to the right backend. [`mcp.rs`](../src/mcp.rs) connects to external MCP servers and exposes their tools through the same loop. [`session_store.rs`](../src/session_store.rs) provides optional filesystem persistence so sessions survive process restarts. Accepted prompts are saved before provider work, so even a first-request failure remains discoverable and resumable; a storage failure prevents the provider request.

## Module Map

**Binary Modules** (adapter runtime):

| Module                                     | Responsibility                                                                                       |
| ------------------------------------------ | ---------------------------------------------------------------------------------------------------- |
| [`acp/`](../src/acp/)                         | ACP transport registration, request handler dispatch, response builders, permission requesters       |
| [`session.rs`](../src/session.rs)             | Adapter-owned session data, mode/permission policy, tool-call accumulation                          |
| [`turn.rs`](../src/turn.rs)                   | Prompt-turn orchestration: LLM streaming, tool-call accumulation, loop control, cancellation         |
| [`tools/`](../src/tools/)                     | Built-in tool registration, execution and filesystem boundaries                                      |
| [`registry.rs`](../src/tools/registry.rs)     | Domain tool registry/executor interfaces, context, categories and results                           |
| [`execution/`](../src/tools/execution)        | Tool definitions, argument parsing, execution (read/write/edit/grep/glob/command), output truncation |
| [`filesystem.rs`](../src/tools/filesystem.rs) | Approved-root path resolution, directory-capability I/O, editor-path validation                     |
| [`search.rs`](../src/tools/search.rs)         | Confined cwd traversal, in-root ignore rules and cancellable file reads                              |
| [`mcp.rs`](../src/mcp.rs)                     | MCP server connection (stdio + HTTP streamable), tool-name mapping, invocation, result rendering     |
| [`session_store.rs`](../src/session_store.rs) | Shared session lifecycle, MCP resource ownership, metadata and JSONL history persistence              |
| [`dev.rs`](../src/dev.rs)                     | Development utilities, smoke tests, CLI testing backends                                             |
| [`error.rs`](../src/error.rs)                 | Unified domain error type (adapter crate root)                                                       |

**Library Modules** (`llm` - reusable client):

| Module                               | Responsibility                                                                 |
| ------------------------------------ | ------------------------------------------------------------------------------ |
| [`llm/types.rs`](../src/llm/types.rs)   | Chat message, request, tool definition, and stream-event types (public facade) |
| [`llm/client.rs`](../src/llm/client.rs) | HTTP client with SSE retry, `LlmClient` trait, `ChatClient` impl               |
| [`llm/stream.rs`](../src/llm/stream.rs) | SSE event parsing, tool-call delta reassembly, finish-reason mapping           |
| [`llm/config.rs`](../src/llm/config.rs) | Environment-driven config (`LLM_API_KEY`, `LLM_BASE_URL`, `LLM_MODEL`)         |
| [`llm/error.rs`](../src/llm/error.rs)   | Typed error enum (config, HTTP, SSE, JSON, transport)                          |

Completion transport recovery is limited to three retries on fresh connections
before any text, thought, tool delta or other completion event has been emitted.
After output, a dropped stream fails without replaying the generation.
[Selected-content Sessions](selected-content.md) permit exactly one
completion POST. Cancelling or dropping a stream releases its HTTP response and
transport task. Completion POSTs do not follow redirects.

An oversized SSE event or a malformed stream fails the completion, including
errors after its finish reason. Lost deltas never become executable tool calls
or completed assistant/tool history. Clean EOF after a valid finish remains
supported, as does valid trailing usage accounting.
Text, thought, or tool deltas and repeated finish reasons after the terminal
finish are rejected. Output in the same chunk as its finish remains valid.

Each completion accepts at most 128 tool calls, with indices from 0 through 127.
This is a local defensive limit that bounds allocation and work driven by provider
indices. Out-of-range indices fail immediately as an invalid provider response,
before any calls in that completion are authorized, executed, or saved to history.
Fragmented and interleaved calls within the limit remain supported.

## Design Principles

- **Translation boundary**: `session`, `turn` and the tool registry interface use adapter-owned inputs, events, capabilities, categories and results. ACP prompt validation, notification encoding, config selectors and permission dialogs live in `acp/`; concrete filesystem/terminal RPCs live at the tool execution edge. The session store owns persistence and MCP handles separately from domain session records.
- **Error presentation**: ACP errors retain their JSON-RPC classification and return fixed provider, storage, or validation diagnostics. Private response values, credentials, URLs, paths, and raw internal causes are omitted; domain errors retain their diagnostic detail internally.
- **Testable seams**: `LlmClient` and `ToolRegistry` let a complete prompt/tool/history/cancellation test run with domain inputs and events, without constructing ACP values or an editor connection. Tool executors bind external services at the existing registry boundary. ACP handler tests separately verify wire shapes and editor behavior.
- **Single async runtime**: Tokio multi-thread throughout. Local file access, search, and history persistence run on the blocking pool. No lock is held across `.await`. No mixing of async runtimes.
- **No unsafe code**: `#![forbid(unsafe_code)]` at every crate root.
