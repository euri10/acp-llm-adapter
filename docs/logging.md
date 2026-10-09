# Logging and debugging

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

Tracing spans for prompt turns, tool dispatch, LLM requests, and session lifecycle handlers carry `session_id`. Startup, model-list discovery, `initialize`, and new-session setup intentionally use `session_id="none"` because no ACP session exists at those entry points; child prompt spans replace that value once a session is established.

When serve logging is enabled, those tracing events are written alongside wire records: session-scoped events go to that session's `log.jsonl`, while unscoped events stay in the connection fallback log. They continue to be emitted to stderr and remain controlled by `RUST_LOG`.

Serve responses expose the absolute session log path as `_meta.logJsonlPath` on `session/new`, `session/load`, `session/resume`, and `session/list`. The field is omitted when structured logging is disabled.
