# Mock backend

`--backend mock` answers without a provider or an API key, so the adapter can be
driven end to end offline. By default it emits a `mock reasoning` thought
chunk, then replies with text.

To exercise the tool-call path — permission prompting, tool-call status updates,
cancellation part-way through a command — include `!tool run_command ` in a
prompt. Everything after the directive is the shell command:

```text
!tool run_command sleep 300
```

`!tool read_file ` works the same way for a `read_file` call; everything after
the directive is the path:

```text
!tool read_file notes.txt
```

The mock answers with the corresponding tool call, streamed as the metadata
and argument deltas a real provider sends, and closes the turn once the tool
result comes back. The directive may occur anywhere in the prompt, but the
value after it must be nonempty. This is a documented affordance of the mock
rather than a test-only hook: it can drive tool execution without a provider
from an editor session as well as from the test suite.
