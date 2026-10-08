# ACP LLM Adapter

Editors may opt into immutable, tool-less
[selected-content Sessions](docs/selected-content.md), independent of Plan or
YOLO mode.

`acp-llm-adapter` is a headless ACP server that exposes LLM providers (DeepSeek, GLM, Groq) as agents to ACP-capable editors.

> [!WARNING]
> This is alpha software. Expect breaking changes, incomplete ACP coverage, and rough edges while the adapter is still being shaped.

## Installation

```bash
cargo install acp-llm-adapter
```

## Editor Setup

### CodeCompanion

CodeCompanion uses ACP adapters for chat interactions. Extend the adapter config with this server and select it for chat.

For an isolated runnable Neovim repro, see [`examples/codecompanion-minimal.lua`](examples/codecompanion-minimal.lua):
`nvim --clean -u examples/codecompanion-minimal.lua`.

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
              "acp-llm-adapter",
              "serve",
              "--backend",
              "deepseek"
            },
          },
          env = {
            LLM_API_KEY = os.getenv "DEEPSEEK_API_KEY",
            RUST_LOG = "acp_llm_adapter::llm=debug",
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

The two binaries provide adapter-owned logging without a shell wrapper:

- `acp-llm-adapter serve --backend <backend>` writes both ACP wire records and internal tracing events for the adapter. Set `ACP_LOG=1` to enable structured logging. Session records are written to `$XDG_STATE_HOME/acp-llm-adapter/sessions/<session-id>/log.jsonl` (or `~/.local/state/acp-llm-adapter/sessions/<session-id>/log.jsonl` when `XDG_STATE_HOME` is unset); records before a session exists use `connections/<connection-id>.jsonl`.
- `acp-proxy -- <agent> [args...]` records both directions of a foreign ACP agent's traffic and its stderr. Proxy logs use the separate `.../acp-llm-adapter/proxy/` root, with the same `connections/` and `sessions/` layout. This keeps foreign sessions out of the adapter's session store.

Both binaries route session-scoped wire frames to their actual session and
correlate replies with requests independently in each direction. Interleaved
sessions do not inherit the most recently created session's log destination.
Unscoped wire records remain in the connection log after sessions are created.

On Linux, the proxy watches its parent's death and adopts orphaned Agent
descendants. Client exit, proxy SIGTERM/SIGINT, or Agent exit terminates and reaps
the remaining tree, including descendants that create their own Unix sessions.
Client stdin EOF allows one second for graceful Agent exit before forced cleanup;
output draining is also bounded. This requires readable `/proc/self/task`.
It does not protect against SIGKILL of the proxy itself or provide hostile-process
containment. Other platforms retain direct-child cleanup only.

> [!IMPORTANT]
> The `--` before the agent command is mandatory: everything after it is
> forwarded verbatim, while anything before it is parsed as an `acp-proxy`
> option. Omitting it makes `acp-proxy` treat the agent name and its flags as
> its own arguments and exit with:
>
> ```text
> error: unexpected argument 'codex-acp' found
> Usage: acp-proxy [OPTIONS] -- <COMMAND>...
> ```
>
> When wiring `acp-proxy` into a client that builds argv as an array, include
> a literal `"--"` element before the agent command, e.g.
> `{ "acp-proxy", "--", "codex-acp" }` rather than
> `{ "acp-proxy", "codex-acp" }`.

Both binaries use the same retention and redaction policy. By default no log is ever removed: logs are primary evidence for filed defects. To bound them, set `ACP_LOG_MAX_BYTES` (aggregate size) and/or `ACP_LOG_MAX_AGE_DAYS` to a positive integer; `unlimited`, `0`, or anything else leaves that axis unbounded. A bound only evicts logs written under a bound: each writer records its bounds beside the log in `<file>.retention` (the most permissive writer wins), and a log with no such record — older logs, or another process's — is never evicted. So an agent configured with a bound can never delete the logs of an agent that runs unbounded, even when both share one log root. Among bounded logs, the size bound is aggregate and evicts oldest-first. Retention only removes `connections/*.jsonl` and session `log.jsonl` files, never session metadata or history.

Before structured records are written, the shared field policy replaces prompts, content, tool arguments/results (including ACP `rawInput`/`rawOutput`), command output, titles, edit text, free-text messages/descriptions, opaque `_meta`/data, and credential-bearing configuration (commands, arguments, environments, headers, URLs, values, and credential fields) with `[REDACTED]`. The same rules cover camelCase ACP fields and snake_case tracing fields. Unknown string fields are also redacted, so newly added content fields cannot silently bypass the policy. Malformed frames and captured stderr remain string records with redacted content. Methods, request/tool/session IDs, status, error codes, timing, numeric diagnostics, and diagnostic strings such as models and file paths remain available; live ACP frames and forwarded stderr are unchanged. With `ACP_LOG_UNREDACTED` unset, empty, or `0`, redaction is enabled. Set `ACP_LOG_UNREDACTED=1` to retain the original content for debugging (any other nonempty value also opts out). Such logs contain full prompts, file contents, command output, and credentials; keep that opt-out limited to active debugging. Existing logs are not rewritten. Set `RUST_LOG` (for example `acp_llm_adapter::llm=trace`) to control tracing output; LLM request bodies are not written at trace, only their serialized byte size.

`usage_update` notifications include cumulative session cost for the models with known published rates: `deepseek-v4-flash` and `deepseek-v4-pro` ([DeepSeek's pricing table](https://api-docs.deepseek.com/quick_start/pricing/)), and `openai/gpt-oss-120b` and `openai/gpt-oss-20b` (Groq's per-model pages, e.g. [gpt-oss-120b](https://console.groq.com/docs/model/openai/gpt-oss-120b)). Cost is calculated from cache-hit, cache-miss, and output tokens; both providers bill prompt-cache reads below the uncached input rate. A model with no known rates reports usage without a cost rather than an invented one.

For local testing or a provider price change, `LLM_PRICING` accepts JSON such as `{"deepseek-v4-pro":{"cache_hit":0.003625,"cache_miss":0.435,"output":0.87}}`, with values in USD per million tokens.

Usage counters are validated before they update telemetry, session cost, or
assistant/tool history. Input/output and cache sums must fit `u64`; a supplied
total must cover input plus output, reasoning tokens must fit within output,
and cache reads plus writes must fit within input. Larger provider totals are
preserved. Unrepresentable per-response or cumulative totals and costs fail the
prompt with a provider error; the session remains usable for the next prompt.
Costs use wide integer arithmetic before conversion to microdollars, avoiding
silent wrapping or saturation. Missing usage or unknown model prices remain
unknown.

Usage may arrive beside a completion or in a separate final accounting frame,
including Groq's `x_groq.usage` envelope. Duplicate envelopes count once;
matching counters are combined with optional details, and conflicting counters
are rejected. Accounting requires explicit input and output counts and never
substitutes for a completion's finish reason.

Tracing spans for prompt turns, tool dispatch, LLM requests, and session lifecycle handlers carry `session_id`. Startup, model-list discovery, `initialize`, and new-session setup intentionally use `session_id="none"` because no ACP session exists at those entry points; child prompt spans replace that value once a session is established.

When serve logging is enabled, those tracing events are written alongside wire records: session-scoped events go to that session's `log.jsonl`, while unscoped events stay in the connection fallback log. They continue to be emitted to stderr and remain controlled by `RUST_LOG`.

Serve responses expose the absolute session log path as `_meta.logJsonlPath` on `session/new`, `session/load`, `session/resume`, and `session/list`. The field is omitted when structured logging is disabled.

## Context-window reporting

The context gauge uses the streamed usage `context_length` when present.
Otherwise it uses the selected model's positive integer `context_window` from
the startup `GET /models` response, then the built-in model table as a fallback.
Discovery metadata is retained in memory and refreshed on adapter startup;
no extra request is made per turn. Missing or invalid sizes leave the fallback
intact. If all sources are unknown, no `usage_update` is emitted.

Optional startup discovery has a two-second deadline covering connection,
response headers, and the full body. Failure keeps the configured default model.
Unix termination handlers are registered before discovery starts, so signals
cancel it immediately. Editor disconnect is observed when this bounded startup
step finishes, within two seconds of starting discovery.

## Architecture

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

**Left side** — the adapter speaks the [Agent Client Protocol](https://agentclientprotocol.com) (ACP) over stdio as JSON-RPC 2.0 frames. The `agent-client-protocol` crate handles the wire protocol; [`acp/`](src/acp/) registers request handlers and translates between ACP schema types and the adapter's internal types.

**Right side** — the adapter speaks HTTPS + Server-Sent Events to the provider's OpenAI-compatible `/chat/completions` endpoint via a thin client owned by this crate in [`src/llm/`](src/llm/). A [`LlmClient`](src/llm/client.rs) trait provides the mock seam for testing without a live API key.

**Middle** — the adapter is the translator _and_ the agent harness. [`turn.rs`](src/turn.rs) orchestrates the prompt→tool-call→execute→feed-back loop. [`tools/`](src/tools/) registers built-in tools (read/write/edit files, glob, grep, shell commands) and routes execution to the right backend. [`mcp.rs`](src/mcp.rs) connects to external MCP servers and exposes their tools through the same loop. [`session_store.rs`](src/session_store.rs) provides optional filesystem persistence so sessions survive process restarts. Accepted prompts are saved before provider work, so even a first-request failure remains discoverable and resumable; a storage failure prevents the provider request.

### Module Map

**Binary Modules** (adapter runtime):

| Module                                     | Responsibility                                                                                       |
| ------------------------------------------ | ---------------------------------------------------------------------------------------------------- |
| [`acp/`](src/acp/)                         | ACP transport registration, request handler dispatch, response builders, permission requesters       |
| [`session.rs`](src/session.rs)             | Adapter-owned session data, mode/permission policy, tool-call accumulation                          |
| [`turn.rs`](src/turn.rs)                   | Prompt-turn orchestration: LLM streaming, tool-call accumulation, loop control, cancellation         |
| [`tools/`](src/tools/)                     | Built-in tool registration, execution and filesystem boundaries                                      |
| [`registry.rs`](src/tools/registry.rs)     | Domain tool registry/executor interfaces, context, categories and results                           |
| [`execution/`](src/tools/execution)        | Tool definitions, argument parsing, execution (read/write/edit/grep/glob/command), output truncation |
| [`filesystem.rs`](src/tools/filesystem.rs) | Approved-root path resolution, directory-capability I/O, editor-path validation                     |
| [`search.rs`](src/tools/search.rs)         | Confined cwd traversal, in-root ignore rules and cancellable file reads                              |
| [`mcp.rs`](src/mcp.rs)                     | MCP server connection (stdio + HTTP streamable), tool-name mapping, invocation, result rendering     |
| [`session_store.rs`](src/session_store.rs) | Shared session lifecycle, MCP resource ownership, metadata and JSONL history persistence              |
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

Completion transport recovery is limited to three retries on fresh connections
before any text, thought, tool delta or other completion event has been emitted.
After output, a dropped stream fails without replaying the generation.
[Selected-content Sessions](docs/selected-content.md) permit exactly one
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

### Design Principles

- **Translation boundary**: `session`, `turn` and the tool registry interface use adapter-owned inputs, events, capabilities, categories and results. ACP prompt validation, notification encoding, config selectors and permission dialogs live in `acp/`; concrete filesystem/terminal RPCs live at the tool execution edge. The session store owns persistence and MCP handles separately from domain session records.
- **Error presentation**: ACP errors retain their JSON-RPC classification and return fixed provider, storage, or validation diagnostics. Private response values, credentials, URLs, paths, and raw internal causes are omitted; domain errors retain their diagnostic detail internally.
- **Testable seams**: `LlmClient` and `ToolRegistry` let a complete prompt/tool/history/cancellation test run with domain inputs and events, without constructing ACP values or an editor connection. Tool executors bind external services at the existing registry boundary. ACP handler tests separately verify wire shapes and editor behavior.
- **Single async runtime**: Tokio multi-thread throughout. Local file access, search, and history persistence run on the blocking pool. No lock is held across `.await`. No mixing of async runtimes.
- **No unsafe code**: `#![forbid(unsafe_code)]` at every crate root.

## Requirements

- Rust stable
- `LLM_API_KEY` (required for the DeepSeek, GLM and Groq backends)
- Optional: `LLM_BASE_URL` (overrides the provider's default base URL)
- Optional: `LLM_MODEL` (overrides the provider's default model)

Select a provider with `--backend deepseek|glm|groq|mock`. On both `serve` and `dev`, `--backend` is required. The `mock` backend requires no API key and is useful for local testing.

The reasoning-effort selector starts at **Provider default**, which omits the
parameter. Explicit choices are sent unchanged: Groq GPT-OSS offers Low, Medium
and High; known DeepSeek models offer Low, High and Max. Options follow the
provider contracts ([Groq](https://console.groq.com/docs/api-reference),
[DeepSeek](https://api-docs.deepseek.com/api/create-chat-completion/)). Other
models, including GLM-4.6, offer only Provider default until their effort contract
is supported. Invalid updates are rejected before changing state; switching or
restoring a model resets an unsupported stored effort to Provider default.

Every live backend is the same OpenAI-compatible client; the backend only chooses the defaults that `LLM_BASE_URL` and `LLM_MODEL` override:

| Backend | Default base URL | Default model |
| --- | --- | --- |
| `deepseek` | `https://api.deepseek.com` | `deepseek-v4-pro` |
| `glm` | `https://api.z.ai/api/paas/v4` | `glm-4.6` |
| `groq` | `https://api.groq.com/openai/v1` | `openai/gpt-oss-120b` |

## Supported Modes

Ordinary sessions receive a concise adapter-owned coding instruction with the
current mode's permission rules on every provider request. It is assembled with
the advertised tools and conversation, never stored or replayed as conversation
history. Plan adds its read-only restrictions. Selected-content sessions retain
their separate tool-less contract and do not receive this coding instruction.

Completed assistant reasoning is retained separately from visible answers in
ordinary session history. DeepSeek requests replay it as `reasoning_content`,
including after tool calls, subsequent prompts and load/resume, as required by
the provider's [thinking-mode contract](https://api-docs.deepseek.com/guides/thinking_mode/#tool-calls).
It counts toward the request-size budget and stays with its assistant/tool unit
when old history is dropped. Other providers' outgoing message formats are
unchanged; selected-content sessions still do not persist their payloads.

History filtering reserves the system instruction and the latest user prompt
within the estimated 256 KiB message budget before considering older messages
and complete assistant/tool units, including their argument strings. A current
prompt that cannot fit is rejected with `current prompt exceeds request size limit`
before provider work or history changes; its text is never silently dropped or
truncated. A subsequent smaller prompt can proceed normally.

- `ask`
- `accept-edits`
- `plan`
- `yolo`

`session/set_mode` switches posture live during a session. In `accept-edits`, edit actions auto-approve while shell actions still prompt. In `yolo`, mutating tools auto-approve.

The current mode governs each subsequent provider request and tool dispatch.
Switching to Plan also prevents pending approvals and file preflight reads from
authorizing a later mutation; an operation already started is not undone.

Ordinary session mode, model, reasoning effort and output-token settings are
persisted before an update succeeds, including changes made between prompts.
Load and resume retain those settings without requiring another prompt first.
A failed save reports a storage error and leaves the previous settings intact.
Selected-content helpers remain ephemeral and retain their immutable limits.

MCP tools are external executors and use the same Execute permission policy as shell commands: `ask` and `accept-edits` request editor approval, while `yolo` auto-approves. Explicit “allow always” and “reject always” decisions apply to that tool name for the current session; a remembered rejection takes precedence over `yolo`. Restoring a session starts with fresh permission decisions. Plan mode and selected-content sessions prohibit MCP execution even with a remembered approval.

Cancelling a turn during MCP approval prevents invocation. Cancelling an in-flight MCP call stops the local wait and sends an MCP cancellation notification, with delivery bounded to one second. A remote server may ignore cancellation, and cancellation cannot undo a side effect that already occurred.

MCP supports stdio, Streamable HTTP, and legacy HTTP+SSE. An SSE server entry
opens its URL with GET, receives the `endpoint` event, posts JSON-RPC to that
message endpoint, and receives responses on the original event stream. Custom
headers apply to both channels. URLs must be HTTP(S) without userinfo or
fragments; the message endpoint must stay on the configured origin, and
redirects are rejected so credentials cannot be forwarded elsewhere. SSE
connection setup, initialization, and tool discovery share a five-second
deadline; each POST acknowledgement has the same bound. Tool execution may
continue until its response or cancellation. The SDK session owns and closes
the event stream. A dropped stream fails instead of reconnecting into a new
legacy session or replaying a potentially mutating tool call.

## Supported Tools

In ordinary sessions, sending `/clear` as the only text block clears both the
in-memory conversation and persisted replay history without calling a provider
or tool. Session settings, permission decisions, title, and cumulative spend are
retained. Clearing an active turn is rejected; a failed disk update leaves the
history intact. In selected-content helpers, `/clear` is literal input and does
not reset their one-attempt limit.

- `read_file`
- `list_dir`
- `glob`
- `grep`
- `write_file`
- `edit_file`
- `run_command`

Tool calls are permission-gated and surfaced through ACP so the editor can show native diffs and command output. A tool call is reported in progress only once its work actually starts — after you approve it — so a command waiting on a permission prompt stays visibly pending rather than appearing to run.

Cancelling during approval ends the wait without starting the tool. Late approval
replies cannot execute the cancelled call or change remembered permissions.
Cancellation also skips remaining calls in the same batch and prevents a file
write after a cancelled preflight read. It cannot undo an operation already sent
to an editor or a write that has already started.

Cancellation during a fragmented provider tool call discards the incomplete
call and returns `cancelled`. A pending editor-backed file read does not keep the
turn active after cancellation: its late reply is ignored and the session can
accept another prompt.

Editor-backed terminal creation, exit waits, and output waits are cancellable.
Kill and release each have a one-second response deadline; release is attempted
even if kill fails. A cancelled creation request remains owned by the ACP
connection: a late terminal ID is killed and released, and disconnect drops the
pending wait. A silent editor may still have a running process; local cleanup
deadlines cannot guarantee remote termination.

Built-in file tools are confined to the session `cwd` and explicitly approved
`additionalDirectories`, regardless of permission mode. Relative paths try `cwd`
first, then additional directories in order; absolute paths must resolve inside
an approved root. Traversal and symlinks cannot grant access outside those roots;
dangling or unverifiable paths fail closed. New files require an allowed parent.
Local reads and writes use directory handles to prevent a symlink replacement
after validation from redirecting I/O outside the approved root.

`glob` and `grep` search only `cwd`, not additional directories. They skip hidden
entries and symlinks, apply nested in-root `.gitignore` and `.ignore` rules, and
never load parent/global Git ignore configuration. An ignore file that cannot
be safely read causes the search to fail. Search traversal and file reads use
the same directory-capability boundary and stop cooperatively on cancellation.

Editor-backed reads and writes are validated before delegation and carry
canonical absolute paths. Roots must be locally verifiable, even for editor I/O.
The trusted editor must preserve confinement when accessing the path: ACP passes
a path, not an atomic filesystem capability.

`edit_file` rereads the document after approval and reports a conflict without
writing if its contents changed while approval was pending. The user can reread
and retry against the updated document. This does not guarantee atomicity against
changes between that final read and the write. `write_file` creates new
editor-managed files when the editor's preflight read returns `ResourceNotFound`;
other read failures prevent the write.

`run_command` remains permission-gated host execution starting in `cwd`, **not a
filesystem sandbox**. MCP tools have their own permission boundary; these file
roots do not sandbox MCP servers.

## Mock Backend

`--backend mock` answers without a provider or an API key, so the adapter can be
driven end to end offline. By default it replies with text.

To exercise the tool-call path — permission prompting, tool-call status updates,
cancellation part-way through a command — prefix a prompt with
`!tool run_command `. Everything after the prefix is the shell command:

```text
!tool run_command sleep 300
```

The mock then answers with a `run_command` tool call, streamed as the metadata
and argument deltas a real provider sends, and closes the turn once the tool
result comes back. This is a documented affordance of the mock rather than a
test-only hook: it is the only way to drive tool execution without a provider,
from an editor session as much as from the test suite.

## ACP Protocol Coverage

Loading or resuming a session with an active turn is rejected at session
publication, including when the turn began during restore setup. Cancellation
continues to target the original turn until its cleanup completes.

Closing or deleting an active session cancels its work and keeps restoration
blocked until that turn finishes cleanup. An older turn cannot clear a newer
turn's cancellation owner or recreate a deleted session's history.

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
| MCP tools (SSE)                                                                             | ✅ Legacy HTTP+SSE: GET events and separate POST messages             |

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
