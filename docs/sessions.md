# Sessions and modes

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

## Clearing history

In ordinary sessions, sending `/clear` as the only text block clears both the
in-memory conversation and persisted replay history without calling a provider
or tool. Session settings, permission decisions, title, and cumulative spend are
retained. Clearing an active turn is rejected; a failed disk update leaves the
history intact. In selected-content helpers, `/clear` is literal input and does
not reset their one-attempt limit.
