# Tools and permissions

## Supported Tools

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

`edit_file` and `write_file` permission requests carry an ACP diff of the whole
file, the exact change that approval will apply, so the editor can show it
before approval. Both tools read the document before asking and again after
approval, and report a conflict without writing if it changed, appeared or was
deleted while approval was pending. The user can reread and retry against the
updated document. This does not guarantee atomicity against changes between that
final read and the write. `write_file` creates new editor-managed files when the
editor's preflight read returns `ResourceNotFound`; other read failures prevent
the write. When the editor writes files but does not offer reads, the old
contents are unknown: the request carries no diff, the write proceeds without the
conflict check, and its result reports text rather than a diff.

`run_command` remains permission-gated host execution starting in `cwd`, **not a
filesystem sandbox**. MCP tools have their own permission boundary; these file
roots do not sandbox MCP servers.

## MCP tools

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
