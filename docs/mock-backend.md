# Mock backend

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
