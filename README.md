# acp-llm-adapter

Use DeepSeek, GLM or Groq models as a coding agent in any editor that speaks the
[Agent Client Protocol](https://agentclientprotocol.com) (ACP), such as Zed or
Neovim with LouiseLM or CodeCompanion. The adapter is the agent: it runs the
tool loop (read, search and edit files, run commands) behind permission prompts,
keeps sessions, and reports usage and cost. The provider only needs an
OpenAI-compatible chat API.

> [!WARNING]
> Alpha software: expect breaking changes. It speaks ACP v1; prompts take text
> and embedded file context, not images or audio, and elicitation is not
> supported ([ACP coverage](docs/acp-coverage.md)).

![LouiseLM chat with acp-llm-adapter: DeepSeek reads hello.py, edits it after approval in Ask mode, and the buffer shows the new docstring](https://raw.githubusercontent.com/euri10/acp-llm-adapter/main/docs/images/louiselm.png)

## Install

```sh
cargo install acp-llm-adapter
acp-llm-adapter --version
```

Requires Rust 1.95 or newer.

## Choose a backend

| `--backend` | Provider | Default model | Provider docs |
| --- | --- | --- | --- |
| `deepseek` | DeepSeek | `deepseek-v4-pro` | [api-docs.deepseek.com](https://api-docs.deepseek.com/) |
| `glm` | Z.ai | `glm-4.6` | [docs.z.ai](https://docs.z.ai/) |
| `groq` | Groq | `openai/gpt-oss-120b` | [console.groq.com/docs](https://console.groq.com/docs/overview) |
| `mock` | none: canned replies, no key | | [mock backend](docs/mock-backend.md) |

The adapter reads the provider key from `LLM_API_KEY`; the snippets below map
your own variable, such as `DEEPSEEK_API_KEY`, onto it. The editor's model
picker lists the provider's models; `LLM_MODEL` and `LLM_BASE_URL` override the
defaults.

## Editor setup

Each snippet is the exact content of a file in [`examples/`](examples/), which
CI runs against a local fake provider (`scripts/test-editor-snippets`). They use
DeepSeek; for another backend change `--backend` and the key.

### LouiseLM

In your [LouiseLM](https://github.com/euri10/louiselm) setup:

<!-- snippet: examples/louiselm.lua -->
```lua
assert(require("louiselm").setup({
  agents = {
    deepseek = {
      provider = "DeepSeek",
      command = "acp-llm-adapter",
      args = { "serve", "--backend", "deepseek" },
      env = { LLM_API_KEY = assert(vim.env.DEEPSEEK_API_KEY, "DEEPSEEK_API_KEY is not set") },
    },
  },
}))
```

Then `:LouiselmChat`, as in the screenshot above. Tested with LouiseLM
`plugin-v0.2.0` on Neovim 0.12.5, 2026-10-09.

### Zed

In Zed's `settings.json`
([Zed external agents](https://zed.dev/docs/ai/external-agents)):

<!-- snippet: examples/zed-settings.json -->
```json
{
  "agent_servers": {
    "DeepSeek (acp-llm-adapter)": {
      "type": "custom",
      "command": "acp-llm-adapter",
      "args": ["serve", "--backend", "deepseek"],
      "env": { "LLM_API_KEY": "your-deepseek-api-key" }
    }
  }
}
```

In the Agent Panel's new-thread menu, pick the external agent
"DeepSeek (acp-llm-adapter)", not Zed's built-in agent. Tested with Zed 1.23.2,
2026-10-09.

![Zed Agent Panel running the DeepSeek (acp-llm-adapter) external agent, which reads and searches hello.py and answers](https://raw.githubusercontent.com/euri10/acp-llm-adapter/main/docs/images/zed.png)

### CodeCompanion

The snippet reuses CodeCompanion's `opencode` ACP preset with this adapter's
command and reads the key from `DEEPSEEK_API_KEY`:

<!-- snippet: examples/codecompanion.lua -->
```lua
require("codecompanion").setup({
  adapters = {
    acp = {
      deepseek_acp = function()
        -- The opencode preset is a plain ACP adapter; reuse it for acp-llm-adapter.
        return require("codecompanion.adapters").extend("opencode", {
          name = "deepseek_acp",
          formatted_name = "DeepSeek (acp-llm-adapter)",
          commands = { default = { "acp-llm-adapter", "serve", "--backend", "deepseek" } },
          env = { LLM_API_KEY = "DEEPSEEK_API_KEY" },
          opts = { vision = false },
        })
      end,
    },
  },
  interactions = { chat = { adapter = "deepseek_acp" } },
})
```

Then `:CodeCompanionChat`. Tested with CodeCompanion `v19.27.0` on Neovim
0.12.5, 2026-10-09.

> [!NOTE]
> CodeCompanion attaches its rules files to every chat by default, including
> `~/.claude/CLAUDE.md` and a project's `AGENTS.md` or `CLAUDE.md`, so their
> contents are sent to your provider. To opt out, add
> `rules = { opts = { chat = { enabled = false } } }` to the setup table.

![CodeCompanion chat with acp-llm-adapter: tool calls, an Approval Required prompt for the edit, and the updated hello.py](https://raw.githubusercontent.com/euri10/acp-llm-adapter/main/docs/images/codecompanion.png)

## What you get

- Permission modes switchable mid-session: `ask`, `accept-edits`, `plan`
  (read-only) and `yolo` ([sessions](docs/sessions.md)).
- Built-in tools `read_file`, `list_dir`, `glob`, `grep`, `write_file`,
  `edit_file` and `run_command`. File tools are confined to the session's
  directory and approved extra directories; `run_command` is permission-gated
  host execution, not a sandbox
  ([tools and permissions](docs/tools-and-permissions.md)).
- MCP servers from the editor over stdio, streamable HTTP or legacy SSE.
- Persisted sessions: list, load with replay, resume, close and delete
  ([ACP coverage](docs/acp-coverage.md)).
- Usage, context-window and cost reporting, with cost for models whose prices
  are published ([usage and cost](docs/usage-and-cost.md)).
- Model and reasoning-effort pickers ([configuration](docs/configuration.md)).

## Configuration

| Variable | Purpose |
| --- | --- |
| `LLM_API_KEY` | Provider key; required except for `mock` |
| `LLM_MODEL` | Override the backend's default model |
| `LLM_BASE_URL` | Override the backend's base URL |
| `ACP_LOG=1` | Write structured per-session logs, redacted by default ([logging](docs/logging.md)) |
| `RUST_LOG` | Tracing filter for stderr, e.g. `acp_llm_adapter=debug` |

## Documentation

[`docs/`](docs/index.md) covers configuration, sessions and modes, tools and
permissions, logging and the `acp-proxy` debugging binary, usage and cost, ACP
coverage, architecture, and the reusable `llm` library API.

## License

MIT OR Apache-2.0.
