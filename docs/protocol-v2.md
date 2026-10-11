# ACP v2 probe

A bounded implementation of the [ACP v2 draft](https://agentclientprotocol.com/protocol/v2/overview),
built to find out what v2 costs this adapter and to report what does not work
while the draft is still open to feedback
([announcement](https://agentclientprotocol.com/announcements/acp-v2-draft)).
It is parked behind an off-by-default cargo feature; released binaries speak
ACP v1 only. Tracked as `daa-acp-v2-37zc`.

## What it covers

Built with the `protocol-v2` feature, `serve` reads each connection's
`initialize` request and routes it with the SDK's `Agent.protocol_router()`:
v1 clients reach the unchanged v1 implementation, v2 clients reach
[`src/acp/v2.rs`](../src/acp/v2.rs).

- The v2 session baseline that `session: {}` advertises: `session/new`,
  `session/list`, `session/resume` (silent, or `replayFrom: start` with the
  same message IDs the prompts reported), `session/close`, `session/prompt`,
  `session/cancel` and session updates. Prompts accept text and resource links;
  links reach the model as Markdown links and are never fetched.
- The split prompt lifecycle: `session/prompt` is answered at prompt
  admission (the prompt passed the size check, claimed the session and was
  persisted, before any provider work) with the user message ID, then the
  `user_message`, `running`, streamed output and `idle` with its stop reason
  follow as updates. A failure after admission ends the turn with the `error`
  stop reason and the sanitized JSON-RPC error; a refusal before admission is a
  request error.
- Config options for mode, model and effort, including
  `session/set_config_option` and `config_option_update`; Plan still revokes a
  pending approval.
- Tools, always run locally because v2 has no client file system or terminal,
  behind the same permission gating as v1; permission prompts become v2
  `session/request_permission` requests with the tool call as subject.
- `/clear`, as in v1.

Not covered: MCP servers, `additionalDirectories`, edits shown with the v2 diff
format, terminals, selected-content Sessions. Unadvertised inputs are refused.

## Trying it

```sh
cargo install --path . --features protocol-v2
cargo test --locked --features protocol-v2 --test serve_protocol_v2
```

The tests drive the real binary over raw JSON-RPC and with the official SDK
v2 client, against the mock backend and a localhost fake provider.

## Result: the architecture held

v2 needed one change outside `src/acp/`: the turn loop now reports prompt
admission, a protocol-neutral event that v1 ignores. Everything else is
translation at the edge onto the existing session store, v1 session handlers,
permission gating and turn loop. No other loop change was needed.

## Findings

Candidates for upstream feedback are marked; each is drafted, checked for
duplicates and posted only after maintainer approval (`daa-acp-v2-37zc.6`).

1. **Upstream candidate (spec question): the session baseline is
   all-or-nothing.** Advertising `session: {}` obliges `session/resume`,
   `session/close` and resource-link prompts
   ([initialization](https://agentclientprotocol.com/protocol/v2/initialization)).
   An agent without persistence cannot expose sessions at all. Cheap for this
   adapter, which already persists sessions, but worth asking whether a
   narrower baseline was considered.
2. **Upstream candidate (spec clarity): ready `idle` versus turn completion.**
   An agent may send `idle` after `session/new` (the SDK's own example does).
   A client that waits for `idle` after sending its first prompt can take that
   ready update for the end of the turn; our wire tests hit exactly this race.
   The prompt lifecycle could state that completion is the `idle` following the
   `running` transition after acceptance, which the SDK example only says in a
   code comment.
3. **Upstream candidate (SDK docs): the agent-side router is undocumented.**
   `Agent.protocol_router().with_v1(...).with_v2(...)` exists in the Rust SDK,
   but its README documents only `Proxy.protocol_router()`, so agent authors may
   not find the supported side-by-side entry point.
4. **Upstream candidate (API ergonomics): flattened config values.**
   `SetSessionConfigOptionRequest` flattens its value (`type` and `value` at the
   top level of the request) while option lists nest values. Our own test sent
   the nested shape first.
5. **Internal: no diff previews for edits.** v2 permission requests carry the
   subject's `ToolCallUpdate`, so a preview needs the new v2 diff format. The
   probe sends none, a step back from v1's previews (0.9.3) until it does.
6. **Internal: `run_command` could use the `command` subject.** v2's
   `CommandPermissionSubject` (command, cwd, tool call ID) fits better than a
   generic tool-call subject.
7. **Not upstream: SDK lag, resolved.** The `error` stop reason (spec #2281)
   was missing from SDK 3.1 / schema 1.10.2; the adapter moved to 3.3.0 / 1.11.0.

Status of each candidate (filed link or reason for not filing) is recorded here
once `daa-acp-v2-37zc.6` completes.
