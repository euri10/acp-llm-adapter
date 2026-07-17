# ACP LLM Adapter

`acp-llm-adapter` is a headless ACP server that exposes LLM providers (DeepSeek, GLM) as agents to ACP-capable editors.

> [!WARNING]
> This is alpha software. Expect breaking changes, incomplete ACP coverage, and rough edges while the adapter is still being shaped.

## Installation

```bash
cargo install acp-llm-adapter
```

## Editor Setup

### CodeCompanion

CodeCompanion uses ACP adapters for chat interactions. Extend the adapter config with this server and select it for chat.

For the DeepSeek backend:

```lua
require("codecompanion").setup({
  adapters = {
    acp = {
      glm_acp = function()
        local helpers = require "codecompanion.adapters.acp.helpers"
        return {
          name = "glm_acp",
          formatted_name = "GLM ACP",
          type = "acp",
          roles = {
            llm = "assistant",
            user = "user",
          },
          commands = {
            default = {
              "acp-llm-adapter",
              "serve",
              "--backend",
              "glm",
            },
          },
          env = {
            LLM_API_KEY = os.getenv "Z_AI_API_KEY",
          },
          defaults = {
            mcpServers = {},
          },
          parameters = {
            protocolVersion = 1,
            clientCapabilities = {
              fs = { readTextFile = true, writeTextFile = true },
            },
            clientInfo = {
              name = "CodeCompanion.nvim with acp-llm-adapter (GLM backend)",
              version = "1.0.0",
            },
          },
          handlers = {
            setup = function(_)
              return true
            end,
            auth = function(_)
              return true
            end,
            form_messages = function(self, messages, capabilities)
              return helpers.form_messages(self, messages, capabilities)
            end,
            on_exit = function(_, _) end,
          },
        }
      end,
      deepseek_acp = function()
        local helpers = require "codecompanion.adapters.acp.helpers"
        return {
          name = "deepseek_acp",
          formatted_name = "DeepSeek ACP",
          type = "acp",
          roles = {
            llm = "assistant",
            user = "user",
          },
          commands = {
            default = {
              "/home/lotso/code/acp-llm-adapter/acp-debug.sh",
              "acp-llm-adapter",
              "serve",
              "--backend",
              "deepseek"
            },
          },
          env = {
            LLM_API_KEY = os.getenv "DEEPSEEK_API_KEY",
            RUST_LOG = "acp_llm_adapter::llm=trace,debug",
          },
          defaults = {
            mcpServers = {},
            timeout = 20000, -- 20 seconds
          },
          parameters = {
            protocolVersion = 1,
            clientCapabilities = {
              fs = { readTextFile = true, writeTextFile = true },
            },
            clientInfo = {
              name = "CodeCompanion.nvim with acp-llm-adapter",
              version = "1.0.0",
            },
          },
          handlers = {
            setup = function(_)
              return true
            end,
            auth = function(_)
              return true
            end,
            form_messages = function(self, messages, capabilities)
              return helpers.form_messages(self, messages, capabilities)
            end,
            on_exit = function(_, _) end,
          },
        }
      end,
    },
  },
})
```

### Zed

Zed can run any ACP-capable agent as an external agent. Put the adapter command and its environment in `settings.json` under `agent_servers`.

```json
{
  "agent_servers": {
    "DeepSeek ACP": {
      "type": "custom",
      "command": "acp-llm-adapter",
      "args": ["serve", "--backend", "deepseek"],
      "env": {
        "LLM_API_KEY": "your-api-key"
      }
    }
  }
}
```

To use GLM instead, change `--backend` to `"glm"`. The API key is read from `LLM_API_KEY`.

If Zed is launched from a GUI app launcher, it may not inherit your shell environment. Set the adapter env vars in Zed's agent server config instead of relying on your terminal session.

> [!WARNING]
> I don't have Zed so this is totally untested

## Debugging

For debugging prefer the included [`acp-debug.sh`](acp-debug.sh) wrapper instead of invoking the adapter binary directly. It keeps normal stdio behavior intact for ACP while appending streams to `.local/state/acp-llm-adapter` using filenames like `20260610-080836-32451-acp-llm-adapter-deepseek-stderr.log` and `20260610-080836-32451-codex-acp-stdout-jsonrpc.log` (`<timestamp>-<pid>-<binary>[-<label>]...`). The label is `ACP_DEBUG_LABEL` when set; otherwise it is auto-derived from `--backend` (supports both `--backend value` and `--backend=value`).

## Architecture

The adapter bridges two independent channels:

```
┌────────────────────────────────────────────────────────────────────────────────────────┐
│                         acp-llm-adapter                                           │
│                                                                                        │
│  Editor ──ACP/stdio──▶ ┌─────────────────┐  ┌─────────────────┐                        │
│  (Zed,      JSON-RPC   │  acp.rs         │  │  llm/*          │                        │
│   Neovim,   frames  ◀──│  ACP transport  │  │  HTTPS + SSE    │──▶ LLM Provider API    │
│   ...)                 │  + request      │  │  client, types, │  │  (DeepSeek / GLM)    │
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

**Left side** — the adapter speaks the [Agent Client Protocol](https://agentclientprotocol.com) (ACP) over stdio as JSON-RPC 2.0 frames. The `agent-client-protocol` crate handles the wire protocol; [`acp/`](src/acp/) registers request handlers and translates between ACP schema types and the adapter's internal types.

**Right side** — the adapter speaks HTTPS + Server-Sent Events to the provider's OpenAI-compatible `/chat/completions` endpoint via a thin client owned by this crate in [`src/llm/`](src/llm/). A [`LlmClient`](src/llm/client.rs) trait provides the mock seam for testing without a live API key.

**Middle** — the adapter is the translator _and_ the agent harness. [`turn.rs`](src/turn.rs) orchestrates the prompt→tool-call→execute→feed-back loop. [`tools.rs`](src/tools.rs) registers built-in tools (read/write/edit files, glob, grep, shell commands) and routes execution to the right backend. [`mcp.rs`](src/mcp.rs) connects to external MCP servers and exposes their tools through the same loop. [`session_store.rs`](src/session_store.rs) provides optional filesystem persistence so sessions survive process restarts.

### Module Map

**Binary Modules** (adapter runtime):

| Module                                     | Responsibility                                                                                       |
| ------------------------------------------ | ---------------------------------------------------------------------------------------------------- |
| [`acp/`](src/acp/)                         | ACP transport registration, request handler dispatch, response builders, permission requesters       |
| [`session.rs`](src/session.rs)             | Session state, permission model, in-memory session store, session lifecycle                          |
| [`turn.rs`](src/turn.rs)                   | Prompt-turn orchestration: LLM streaming, tool-call accumulation, loop control, cancellation         |
| [`tools/`](src/tools/)                     | Built-in tool execution with two submodules:                                                         |
| [`registry.rs`](src/tools/registry.rs)     | `ToolRegistry` trait, `ToolContext`, `AdapterToolRegistry` impl, tool metadata                       |
| [`execution/`](src/tools/execution)        | Tool definitions, argument parsing, execution (read/write/edit/grep/glob/command), output truncation |
| [`mcp.rs`](src/mcp.rs)                     | MCP server connection (stdio + HTTP streamable), tool-name mapping, invocation, result rendering     |
| [`session_store.rs`](src/session_store.rs) | Filesystem-backed session metadata and JSONL chat-history persistence                                |
| [`dev.rs`](src/dev.rs)                     | Development utilities, smoke tests, CLI testing backends                                             |
| [`error.rs`](src/error.rs)                 | Unified domain error type (adapter crate root)                                                       |

**Library Modules** (`llm` - reusable client):

| Module                               | Responsibility                                                                 |
| ------------------------------------ | ------------------------------------------------------------------------------ |
| [`llm/types.rs`](src/llm/types.rs)   | Chat message, request, tool definition, and stream-event types (public facade) |
| [`llm/client.rs`](src/llm/client.rs) | HTTP client with SSE retry, `LlmClient` trait, `ChatClient` impl               |
| [`llm/stream.rs`](src/llm/stream.rs) | SSE event parsing, tool-call delta reassembly, finish-reason mapping           |
| [`llm/config.rs`](src/llm/config.rs) | Environment-driven config (`LLM_API_KEY`, `LLM_BASE_URL`, `LLM_MODEL`)         |
| [`llm/error.rs`](src/llm/error.rs)   | Typed error enum (config, HTTP, SSE, JSON, transport)                          |

### Design Principles

- **Translation boundary**: ACP and HTTP types stay at their respective edges. Business logic in the adapter core (`turn`, `tools`, `session_store`) depends only on the adapter's own types — not on `agent-client-protocol` schema types or raw HTTP types.
- **Testable seams**: The `LlmClient` trait lets prompt-turn tests run against canned SSE fixtures without a network. The `ToolRegistry` trait lets tool-loop tests inject fake tools. ACP handler tests use in-memory fake client connections.
- **Single async runtime**: Tokio multi-thread throughout. No lock is held across `.await`. No mixing of async runtimes.
- **No unsafe code**: `#![forbid(unsafe_code)]` at every crate root.

## Requirements

- Rust stable
- `LLM_API_KEY` (required for both DeepSeek and GLM backends)
- Optional: `LLM_BASE_URL` (overrides the provider's default base URL)
- Optional: `LLM_MODEL` (overrides the provider's default model)

Select a provider with `--backend deepseek|glm|mock`. On both `serve` and `dev`, `--backend` is required. The `mock` backend requires no API key and is useful for local testing.

## Supported Modes

- `ask`
- `accept-edits`
- `yolo`

`session/set_mode` switches posture live during a session. In `accept-edits`, edit actions auto-approve while shell actions still prompt. In `yolo`, mutating tools auto-approve.

## Supported Tools

- `read_file`
- `write_file`
- `edit_file`
- `run_command`

Tool calls are permission-gated and surfaced through ACP so the editor can show native diffs and command output.

For sessions that advertise `additionalDirectories`, relative file paths resolve against the
session `cwd` first and then each additional directory in order. Absolute paths are passed
through unchanged, and `run_command` runs as a regular shell command rooted at `cwd` rather
than a filesystem sandbox.

## ACP Protocol Coverage

| Feature                                                                                     | Status                                                                |
| ------------------------------------------------------------------------------------------- | --------------------------------------------------------------------- |
| `initialize`                                                                                | ✅ Full                                                               |
| `authenticate`                                                                              | ✅ No-op (no auth required)                                           |
| `session/new`                                                                               | ✅ Full (async path with MCP startup)                                 |
| `session/list`                                                                              | ✅ Full                                                               |
| `session/close`                                                                             | ✅ Full                                                               |
| `session/delete`                                                                            | ✅ Full                                                               |
| `session/load`                                                                              | ✅ Full (restores persisted state and replays history)                |
| `session/resume`                                                                            | ✅ Full (restores persisted state without replay)                     |
| `session/prompt`                                                                            | ✅ Full (text-only, tool loop, cancellation, plan/thought streaming)  |
| `session/cancel`                                                                            | ✅ Full                                                               |
| `session/set_mode`                                                                          | ✅ Full                                                               |
| `session/set_config_option`                                                                 | ✅ Full                                                               |
| `session/request_permission`                                                                | ✅ Full                                                               |
| `agent_plan` / `current_mode_update` / `config_option_update` / `available_commands_update` | ✅ Emitted                                                            |
| `session_info_update`                                                                       | ✅ Emitted                                                            |
| `logout`                                                                                    | ✅ No-op                                                              |
| `fs/read_text_file`                                                                         | ✅ Client fs or local fallback                                        |
| `fs/write_text_file`                                                                        | ✅ Client fs or local fallback                                        |
| `terminal/*`                                                                                | ✅ Used for `run_command` when the client advertises terminal support |
| MCP tools (stdio)                                                                           | ✅ Full                                                               |
| MCP tools (streamable HTTP)                                                                 | ✅ Full                                                               |
| MCP tools (SSE)                                                                             | ✅ Full                                                               |

## Current Limitations

- No TUI
- No auto model router
- No `apply_patch`-style edits in v0.1

## Library API

The crate also exposes a reusable `llm` module for request construction and
streaming response handling. Generate the API docs locally with:

```bash
cargo doc --no-deps
```

Typical library entry points:

- `llm::ChatMessage` for system, user, assistant, and tool-result messages
- `llm::ChatRequest` for model/tool request construction
- `llm::ToolDefinition` for JSON-schema tool advertisement
- `llm::StreamEvent` for normalized streamed output
- `llm::ChatClient` for HTTP-backed streaming requests

Minimal streaming example:

```rust,no_run
use acp_llm_adapter::llm::{ChatMessage, ChatRequest, ChatClient, LlmClient};
use futures_util::StreamExt;
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = ChatClient::from_env()?;
    let request = ChatRequest::new(vec![ChatMessage::user("Summarize this repository")]);
    let mut stream = client.stream_chat(request, CancellationToken::new())?;

    while let Some(event) = stream.next().await {
        println!("{:?}", event?);
    }

    Ok(())
}
```
