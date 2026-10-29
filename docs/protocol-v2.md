# ACP v2 support

An implementation of the [ACP v2 draft](https://agentclientprotocol.com/protocol/v2/overview)
alongside ACP v1, extending the original `daa-acp-v2-37zc` probe. Promotion is
tracked as `daa-jn8o`. The protocol and the SDK's `unstable_protocol_v2` surface
are still drafts; editor adoption does not imply protocol stability.

## Status: implementation ready, default promotion awaiting editor validation

The original promotion triggers, checked at every SDK upgrade, were:

- ACP announces v2 as stable.
- The SDK stabilises v2: `unstable_protocol_v2` is removed, or V2 is available
  without it.
- A target editor (Zed, CodeCompanion, LouiseLM) requests `protocolVersion: 2`.

Checked 2026-10-29 with SDK 3.5.0: the editor trigger has
fired. [Zed's ACP beta path](https://github.com/zed-industries/zed/blob/e5bdd01cd819d6348ae764e69218adfee3d7edab/crates/agent_servers/src/acp.rs#L132)
[requests version 2](https://github.com/zed-industries/zed/blob/e5bdd01cd819d6348ae764e69218adfee3d7edab/crates/agent_servers/src/acp/v2.rs#L427).
The SDK still gates v2 behind `unstable_protocol_v2`, ACP still lists it as
Draft, and the inspected CodeCompanion and LouiseLM clients request version 1.
The maintainer approved shipping both protocols by default after validation.
The tool/session gaps below are implemented and covered by wire and SDK tests.
LouiseLM and CodeCompanion's unchanged configurations are tested against the
dual-protocol binary and continue negotiating v1.

The remaining editor gate is a real v2 session. Zed 1.27.0-pre includes the v2
client, but its [release build only honors feature-flag overrides for staff](https://github.com/zed-industries/zed/blob/1a91814bd79e80e8f21ed7bea8e228852cd13def/crates/feature_flags/src/store.rs#L99).
An isolated check with `acp-beta: on` still negotiated v1. That is a v1 pass,
not a v2 pass. Until this gate is resolved, `protocol-v2` remains opt-in and
default/released builds continue speaking v1 only.

Once promoted, LouiseLM needs no configuration change: update the adapter
binary, keeping the same command, arguments, backend and environment. The
editor chooses the protocol in `initialize`; enabling v2 in the binary does
not force v1 clients onto v2.

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
- Built-in tools, always run locally because v2 has no client file system or terminal delegation,
  behind the same permission gating as v1; permission prompts become v2
  `session/request_permission` requests. The sequential turn reports
  `requires_action` while waiting, then `running` when it resumes. Cancellation
  ends with `idle(cancelled)`; late approvals cannot resume cancelled work.
- `run_command` uses the command subject with its actual command, cwd and tool
  call ID. MCP calls keep tool-call subjects, even if their arguments contain
  a field named `command`.
- File creation and editing previews/results use v2 changes-list diffs with
  absolute paths and renderable Git patches. Whole-file hunks preserve newline
  endings and quote unusual filenames; unchanged content needs no patch.
- Stdio and streamable-HTTP MCP, using the shared executor and approval policy.
  Unsupported transports (including v1's legacy SSE) are rejected. MCP
  configuration credentials are not stored in v2 session metadata; clients
  supply the desired servers again on resume.
- `additionalDirectories`, with the shared directory-relative confinement.
  Resume applies the newly supplied roots and servers, including empty lists;
  it does not silently reinstate removed access from persistence. It reports an
  idle snapshot after optional history replay.
- `/clear`, as in v1.

Not advertised: image/audio/embedded prompt content, terminal streaming,
elicitation, subagents, selected-content Sessions or MCP-over-ACP. Unadvertised
prompt content and MCP transports are refused.

## Trying it

```sh
cargo install --path . --features protocol-v2
cargo test --locked --features protocol-v2 --test serve_protocol_v2
```

The tests drive the real binary over raw JSON-RPC and with the official SDK
v2 client, against the mock backend and a localhost fake provider. Both MCP
transports, approval/denial, cancellation with a late approval, edit previews,
and root/server revocation on resume are covered. The editor smoke script
explicitly builds the dual-protocol router for the existing v1 clients.
Unknown/inactive sessions return v2's `-32002`; cancellation of an inactive
session is ignored. Corrupt storage remains an internal error, not a missing
session. The v1 error mapping is preserved.

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
5. **Resolved during promotion: `requires_action` during permission waits.** The prompt
   lifecycle says the agent *should* report `requires_action` while blocked on
   a permission response and `running` when work resumes. The probe sends
   neither; the implementation now reports both, with a cancellation regression.
6. **Resolved during promotion: diff previews for edits.** v2 permission requests carry the
   subject's `ToolCallUpdate`, so a preview needs the new v2 diff format. The
   probe sent none; previews and completed edits now share the same v2 encoder.
7. **Resolved during promotion: `run_command` uses the `command` subject.** v2's
   `CommandPermissionSubject` (command, cwd, tool call ID) fits better than a
   generic tool-call subject.
8. **Resolved: SDK lag.** The `error` stop reason (spec #2281) was missing from
   SDK 3.1 / schema 1.10.2; the adapter moved to 3.3.0 / 1.11.0.

Findings 5 to 7 are tracked in `daa-acp-v2-followups-5m7j`; `daa-jn8o` retains
the final default-build/editor-validation gate.
