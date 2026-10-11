# ACP v2 probe

A bounded implementation of the [ACP v2 draft](https://agentclientprotocol.com/protocol/v2/overview),
built to find out what v2 costs this adapter and to report what does not work
while the draft is still open to feedback
([announcement](https://agentclientprotocol.com/announcements/acp-v2-draft)).
It is parked behind an off-by-default cargo feature; released binaries speak
ACP v1 only. Tracked as `daa-acp-v2-37zc`.

## Status: parked

The probe stays behind the `protocol-v2` feature until one of these triggers
fires, checked at every `agent-client-protocol` upgrade
([Rust policy](agent-rust.md)):

- ACP announces v2 as stable.
- The SDK stabilises v2: `unstable_protocol_v2` is removed, or V2 is available
  without it.
- A target editor (Zed, CodeCompanion, LouiseLM) requests `protocolVersion: 2`.

Last checked 2026-10-10, on the upgrade to SDK 3.3.0: none had fired. v2 was
still behind `unstable_protocol_v2`, ACP listed it as Draft, and CodeCompanion
and LouiseLM requested version 1. Promotion means a new design: the follow-ups
in `daa-acp-v2-followups-5m7j`, MCP, edits with the v2 diff format, and whether
released binaries ship v2.

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

Each candidate for upstream feedback was checked against existing issues,
discussions, RFDs and documentation before drafting (`daa-acp-v2-37zc.6`).
None survived, so nothing was filed.

1. **Answered by an RFD: the session baseline is all-or-nothing.** Advertising
   `session: {}` obliges `session/resume`, `session/close` and resource-link
   prompts. This is deliberate: the
   [Required Session Methods RFD](https://agentclientprotocol.com/rfds/v2/required-session-methods)
   notes that almost all agents already support these methods, users expect
   them, and agents without sessions omit `session` entirely.
2. **Answered by the spec: ready `idle` versus turn completion.** An agent may
   report `idle` after `session/new`. The
   [prompt lifecycle](https://agentclientprotocol.com/protocol/v2/prompt-lifecycle)
   requires a stop reason whenever the `idle` transition ends foreground work,
   so a client tells the two apart by the stop reason. Our wire tests briefly
   raced on this; the fix was in the test.
3. **Documented: the agent-side router.** `Agent.protocol_router()` is
   documented, with an example, in the Rust SDK's
   [Protocol V2 guide](https://agentclientprotocol.github.io/rust-sdk/protocol-v2.html),
   which the crate README links; only the README's summary names just the proxy
   router.
4. **Documented: flattened config values.** `session/set_config_option` puts
   `type` and `value` at the top level of the request, as the
   [config options page](https://agentclientprotocol.com/protocol/v2/session-config-options)
   shows. Our test sent a nested value first.
5. **Internal: no `requires_action` during permission waits.** The prompt
   lifecycle says the agent *should* report `requires_action` while blocked on
   a permission response and `running` when work resumes. The probe sends
   neither.
6. **Internal: no diff previews for edits.** v2 permission requests carry the
   subject's `ToolCallUpdate`, so a preview needs the new v2 diff format. The
   probe sends none, a step back from v1's previews (0.9.3) until it does.
7. **Internal: `run_command` could use the `command` subject.** v2's
   `CommandPermissionSubject` (command, cwd, tool call ID) fits better than a
   generic tool-call subject.
8. **Resolved: SDK lag.** The `error` stop reason (spec #2281) was missing from
   SDK 3.1 / schema 1.10.2; the adapter moved to 3.3.0 / 1.11.0.

Findings 5 to 7 are recorded as follow-up work for promotion to full support.
