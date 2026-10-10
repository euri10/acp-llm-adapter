# ACP protocol coverage

acp-llm-adapter implements **ACP v1** (`protocolVersion: 1`), the stable
protocol, and every link below points to the [v1 specification](https://agentclientprotocol.com/protocol/v1/overview).
ACP v2 is still labelled draft and drops v1 surfaces this adapter uses (client
file system, terminals and session modes; see the
[v2 migration guide](https://agentclientprotocol.com/protocol/v2/migration)).
A client that asks for version 2 is answered with 1, as
[version negotiation](https://agentclientprotocol.com/protocol/v1/initialization#version-negotiation) prescribes, and
may continue with v1 or disconnect.

A draft ACP v2 probe exists behind the off-by-default `protocol-v2` cargo
feature (daa-acp-v2-37zc). Built with it, `serve` routes each connection by
its `initialize` request: v1 clients reach this unchanged implementation, and
v2 clients a separate one implementing the v2 session baseline (new, list,
resume with optional replay, close, prompt, cancel and updates) over the same
session store and turn loop; tool execution and config options are not wired
yet. Released binaries do not include it.

Loading or resuming a session with an active turn is rejected at session
publication, including when the turn began during restore setup. Cancellation
continues to target the original turn until its cleanup completes.

Closing or deleting an active session cancels its work and keeps restoration
blocked until that turn finishes cleanup. An older turn cannot clear a newer
turn's cancellation owner or recreate a deleted session's history.

| Feature | Status |
| --- | --- |
| [`initialize`](https://agentclientprotocol.com/protocol/v1/initialization) | ✅ Full |
| [`authenticate`](https://agentclientprotocol.com/protocol/v1/authentication) | ✅ No-op (no auth required) |
| [`session/new`](https://agentclientprotocol.com/protocol/v1/session-setup#creating-a-session) | ✅ Full (async path with MCP startup) |
| [`session/list`](https://agentclientprotocol.com/protocol/v1/session-list#listing-sessions) | ✅ Full |
| [`session/close`](https://agentclientprotocol.com/protocol/v1/session-setup#closing-active-sessions) | ✅ Full |
| [`session/delete`](https://agentclientprotocol.com/protocol/v1/session-delete#deleting-a-session) | ✅ Full |
| [`session/load`](https://agentclientprotocol.com/protocol/v1/session-setup#loading-sessions) | ✅ Full (restores persisted state and replays history) |
| [`session/resume`](https://agentclientprotocol.com/protocol/v1/session-setup#resuming-sessions) | ✅ Full (restores persisted state without replay) |
| [`session/prompt`](https://agentclientprotocol.com/protocol/v1/prompt-turn) | ✅ Full (text-only, tool loop, cancellation, plan/thought streaming) |
| [`session/cancel`](https://agentclientprotocol.com/protocol/v1/prompt-turn#cancellation) | ✅ Full |
| [`session/set_mode`](https://agentclientprotocol.com/protocol/v1/session-modes#setting-the-current-mode) | ✅ Full |
| [`session/set_config_option`](https://agentclientprotocol.com/protocol/v1/session-config-options#setting-a-config-option) | ✅ Full |
| [`session/request_permission`](https://agentclientprotocol.com/protocol/v1/tool-calls#requesting-permission) | ✅ Full |
| [`plan`](https://agentclientprotocol.com/protocol/v1/agent-plan) | ✅ Emitted |
| [`current_mode_update`](https://agentclientprotocol.com/protocol/v1/session-modes#from-the-agent) | ✅ Emitted |
| [`config_option_update`](https://agentclientprotocol.com/protocol/v1/session-config-options#from-the-agent) | ✅ Emitted |
| [`available_commands_update`](https://agentclientprotocol.com/protocol/v1/slash-commands#advertising-commands) | ✅ Emitted |
| [`session_info_update`](https://agentclientprotocol.com/protocol/v1/session-list#updating-session-metadata) | ✅ Emitted |
| [`logout`](https://agentclientprotocol.com/protocol/v1/authentication#logging-out) | ✅ No-op |
| [`fs/read_text_file`](https://agentclientprotocol.com/protocol/v1/file-system#reading-files) | ✅ Client fs or local fallback |
| [`fs/write_text_file`](https://agentclientprotocol.com/protocol/v1/file-system#writing-files) | ✅ Client fs or local fallback |
| [`terminal/*`](https://agentclientprotocol.com/protocol/v1/terminals) | ✅ Used for `run_command` when the client advertises terminal support |
| [MCP tools (stdio)](https://agentclientprotocol.com/protocol/v1/session-setup#stdio-transport) | ✅ Full |
| [MCP tools (streamable HTTP)](https://agentclientprotocol.com/protocol/v1/session-setup#http-transport) | ✅ Full |
| [MCP tools (SSE)](https://agentclientprotocol.com/protocol/v1/session-setup#sse-transport) | ✅ Legacy HTTP+SSE: GET events and separate POST messages |
| [Image and audio prompt content](https://agentclientprotocol.com/protocol/v1/initialization#prompt-capabilities) | ❌ Not advertised: `promptCapabilities` sets only `embeddedContext` |
| [Elicitation](https://agentclientprotocol.com/protocol/v1/elicitation) | ❌ Not supported |
