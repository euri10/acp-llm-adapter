# agentic.nvim: one custom ACP provider

Use the adapter's DeepSeek backend from agentic.nvim with the maintained
[Lua setup](../examples/agentic.lua). No agentic preset or upstream patch is
required. The provider key is read from the launching environment, not stored
in the Lua file.

## Tested boundary

Checked on 2026-11-06, Linux x86_64, adapter **v0.10.0** (runtime source
`1bc1e27`), agentic.nvim
[`f0bef086b3ed6121d9a18a5a820d071c69f2f0f5`](https://github.com/carlos-algms/agentic.nvim/tree/f0bef086b3ed6121d9a18a5a820d071c69f2f0f5),
Neovim **0.12.5** and **0.13.0-dev-1511+g5209695703**. The installation route
was a locked Cargo build and the pinned plugin source archive.

The offline checks run the **actual client, adapter, permission UI buffers, and file
tools**, using a localhost synthetic OpenAI-compatible provider and a dummy
key. They do not establish live DeepSeek quality, human visual review, or
outside-user adoption. GLM/Groq were not run in this client.

| Scenario | Observed result |
| --- | --- |
| Start | ACP v1; custom command/args; environment key accepted by fixture; `ask` mode |
| Read/search/edit | Read `hello.py`, find `def greet`, add a docstring after approval |
| Preview | Displayed split has the full original and intended new file; untouched code remains |
| Deny | Reject a write; `denied.txt` remains absent |
| Cancel | Stop an approved running shell before its delayed write; wait beyond the delay; next prompt works |
| Settings | Shift-Tab changes `ask` → `plan` → `ask`; model picker changes the model in the next provider request |
| Continue | Follow-up after cancellation; hide/show panel; destroy widget and restore via the saved-session picker, retaining id, transcript, model, and history in the next provider request |

The pinned client advertises no editor filesystem or terminal capability;
the adapter executes these tools locally. Built-in file tools stay within the
approved roots. `run_command` is permission-gated host execution, **not an OS
filesystem sandbox**. The mock check runs it only in a disposable project.
Long-history restoration, restoration across editor restarts, ACP v2, attachments,
and other backends are outside this recipe's evidence.

Shutdown caveat: the pinned client's forced-stop path can leave a dead,
unreaped adapter process under a Linux subreaper. The checker closes the
adapter's stdin and waits for its exit before releasing the transport; this
does not fix upstream's normal forced-stop path. No command remains running
in the observed cases. Tracked locally as `daa-agentic-stop-reap-6bjc`.

## Separate live pilot

On the same date, an agent-operated interactive Neovim 0.12.5 run used the
**real DeepSeek API**, model `deepseek-v4-pro`, Low reasoning effort and a
4096-token output cap. The operator issued each action after inspecting the
client state, displayed buffers and disk. This was separate from the automated
tests and was not an independent human-user trial.

In a disposable `hello.py` project, the model read the file, searched for
`greet`, and proposed adding `"""Return a friendly greeting."""`. The displayed
split preserved every other line; disk stayed unchanged until **Allow once**,
then contained exactly that edit. A separate `write_file` request was rejected
with **Reject once**; `denied.txt` remained absent. An approved command created
`live-started`, then slept for 20 seconds before a proposed second write.
Cancellation during the sleep prevented `live-cancelled-too-late`; it was still
absent more than 20 seconds later.

The same session answered a follow-up after hiding/showing the panel. After
destroying its idle widget, selecting it from the real restore picker preserved
its id/model/mode. A new live answer correctly recalled the exact docstring,
rejected filename and cancelled command. Graceful stdin EOF reaped the adapter
before Neovim exited. This proves this small live flow, not model quality on
other tasks, another backend, or return use by an outside user.

Observed friction: the restore picker title starts with injected environment
metadata rather than the task. Select by session id if needed. This is tracked
as `daa-agentic-session-title-4hj7`; the title policy needs a separate design
decision because other clients put context before the user's request.

## Run a small pilot

From this adapter checkout, with Rust 1.95+, Neovim, curl, and tar installed:

```sh
cargo build --locked --bin acp-llm-adapter
export PATH="$PWD/target/debug:$PATH"
export AGENTIC_RECIPE="$PWD/examples/agentic.lua"
export AGENTIC_PILOT="$(mktemp -d)"
curl -fsSL https://codeload.github.com/carlos-algms/agentic.nvim/tar.gz/f0bef086b3ed6121d9a18a5a820d071c69f2f0f5 |
  tar -xz -C "$AGENTIC_PILOT"
export AGENTIC_PLUGIN="$AGENTIC_PILOT/agentic.nvim-f0bef086b3ed6121d9a18a5a820d071c69f2f0f5"
mkdir -p "$AGENTIC_PILOT/project"
printf 'A small pilot project.\n' > "$AGENTIC_PILOT/project/notes.txt"
: "${DEEPSEEK_API_KEY:?Set your key securely in the launching environment}"
unset LLM_API_KEY LLM_BASE_URL LLM_MODEL ACP_LOG ACP_LOG_UNREDACTED
export XDG_STATE_HOME="$AGENTIC_PILOT/state"
export XDG_CACHE_HOME="$AGENTIC_PILOT/cache"
cd "$AGENTIC_PILOT/project"
nvim --noplugin --cmd "set rtp^=$AGENTIC_PLUGIN" -u "$AGENTIC_RECIPE" notes.txt
```

Set `DEEPSEEK_API_KEY` before running this block; do not put a key in shell
history, a screenshot, the recipe file, or a feedback post. This pilot uses
the real provider and incurs normal API usage. Keep debug logging off.
For your existing plugin manager, install the same pinned agentic.nvim commit
and run the contents of `examples/agentic.lua` after the plugin loads.

Open the panel with `:lua require("agentic").open()`. Enter this in its prompt
buffer and submit with Enter in normal mode (or Ctrl-S):

```text
Read notes.txt and search for "pilot". Propose a one-line summary, then ask
permission to create summary.txt with it. Do not run shell commands or edit
other files.
```

Inspect the preview before selecting **Allow once** using the number shown
beside that choice. Check `summary.txt` on disk. Ask for `denied.txt`, select
**Reject once**, and check it is absent. Keep the mode at `ask`; Shift-Tab
opens the mode selector. The model selector is `<localLeader>m` (normally
`\m`); available models come from the provider. The offline check proves the
control path, not live model availability.

Use `:lua require("agentic").stop_generation()` during a turn, then send a
follow-up. `:lua require("agentic").close()` hides the panel; `open()` shows
it again. After an idle session, `destroy_session()` removes its host widget;
`restore_session()` opens the saved-session picker. The tested short history
reappears and accepts another prompt. A real model chooses its own tools, so
the synthetic test's exact sequence is not a promise about the live pilot.

Then choose **one real task of your own** in a disposable project copy. Feedback
should include versions/OS, install route and friction, task outcome, what
worked better or worse than your current workflow, and whether you used it
again. Failed installation or choosing another tool is useful evidence too.
Do not share keys, private project contents, or private conversations.

## Reproduce the offline check

```sh
./scripts/test-agentic
```

The script downloads the pinned plugin, builds the adapter and existing fake
provider, loads the example unchanged, and drives the real client's mappings.
It uses a clean child environment, isolated state/project, dummy key, localhost
URL, and a bounded timeout. It retains synthetic `chat.txt`, the preview's
new file, provider log, and project path in the printed temporary directory.
Neovim removes the temporary project itself when it exits; file effects and
absence checks run before teardown.
It stops and reaps its provider and the client-owned adapter on exit. No
automated test calls a live provider. No adapter runtime behavior changed. The existing fake
provider gained `!history` and `!model` probes; their new assertions were run
red before adding the probes, then green, including a missing-history control.
The documentation itself needs no artificial failing product test.

Upstream references: [custom providers](https://github.com/carlos-algms/agentic.nvim/tree/f0bef086b3ed6121d9a18a5a820d071c69f2f0f5#customizing-acp-providers),
[contribution policy](https://github.com/carlos-algms/agentic.nvim/blob/f0bef086b3ed6121d9a18a5a820d071c69f2f0f5/CONTRIBUTING.md).
