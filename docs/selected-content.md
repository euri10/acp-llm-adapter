# Selected-content Sessions

The adapter advertises `{ "version": 1 }` under
`agentCapabilities._meta["io.github.euri10.louiselm.selectedContent"]`.
An editor may request an immutable contract in `session/new._meta` under the
same key, with positive integer limits:

```json
{"version":1,"input_bytes":131072,"output_bytes":65536,"max_tokens":4096,"timeout_ms":30000}
```

Input is capped at 131072 bytes, combined answer/thought output at 65536 bytes,
output tokens at 8192 and the local deadline at 30000 milliseconds. The closed
record refuses unknown fields, unsupported versions, MCP servers and additional
directories before MCP setup. The response acknowledges exact limits under
the same key. Sessions without the contract retain ordinary permission modes.

The contract is independent of mutable modes: even YOLO and Plan cannot grant
tools. The registry advertises no tools and refuses direct execution. Streaming
rejects every tool delta before accumulation or execution. One prompt attempt
and one Model request are permitted; failure, cancellation or timeout cannot
become a retry or tool-using follow-up. The single-send policy reaches the HTTP
sender: SSE reconnects, server `retry` directives, HTTP retries and redirects
cannot send another completion POST. An incomplete response fails even if it
already emitted text. Only text snapshots are accepted, with
complete input validated before admission; no source is read or truncated.
The token cap survives clearing or increasing the mutable option.

Streaming caps apply before chunks reach ACP. Limit/deadline refusal drops the
owned stream and cancels its token, with no extra task. Cancellation establishes
neither upstream billing nor refunds. No usage event means unknown usage.
Helpers persist neither raw history nor source-derived titles and cannot be
restored as ordinary persisted Sessions. Explicit protocol logging uses the
existing logging/redaction policy. When logging is enabled, `session/new._meta`
includes `logJsonlPath` alongside the acknowledged limits, and `session/list`
includes the same log path. Neither response advertises helper `historyJsonlPath`.

Editors own Provider disclosure authority, qualification, the whole-job/parent
allowance, answer/reference validation and adapter-process disposal. This is
tool confinement in the maintained harness, not kernel containment of arbitrary
Agent executables or certification of Model answer quality.

`cargo test --locked selected_content` covers actual registry/streaming denial,
creation refusal, input/answer/thought limits, immutable token caps,
single-attempt behavior, absent usage, persistence and deadlines with offline
fake Provider streams and the shipped `serve` binary against localhost HTTP
fixtures. `cargo test --test serve_stream_retries` also checks that ordinary
Sessions retain at most three retries before any completion event, never append
a replayed generation, and release pending responses on cancellation or drop.
The complete adapter gates remain required.
