---@diagnostic disable: missing-fields

-- Minimal Neovim repro for CodeCompanion.nvim + acp-llm-adapter.
--
-- Run from this repo with:
--   nvim --clean -u examples/codecompanion-minimal.lua
--
-- Defaults to deepseek_acp. For no-API-key testing, change the chat adapter
-- below to "mock_acp".

vim.env.LAZY_STDPATH = ".repro"
load(vim.fn.system("curl -s https://raw.githubusercontent.com/folke/lazy.nvim/main/bootstrap.lua"))()

local function acp_adapter(opts)
  local helpers = require("codecompanion.adapters.acp.helpers")
  local command = {
    "acp-llm-adapter",
    "serve",
    "--backend",
    opts.backend,
  }

  -- To capture adapter stdio logs from this repo checkout, use:
  -- command = { "./acp-debug.sh", "acp-llm-adapter", "serve", "--backend", opts.backend }

  return {
    name = opts.name,
    formatted_name = opts.formatted_name,
    type = "acp",
    roles = {
      llm = "assistant",
      user = "user",
    },
    commands = {
      default = command,
    },
    env = opts.api_key_env and {
      LLM_API_KEY = os.getenv(opts.api_key_env),
    } or {},
    defaults = {
      mcpServers = {},
    },
    parameters = {
      protocolVersion = 1,
      clientCapabilities = {
        fs = { readTextFile = true, writeTextFile = true },
      },
      clientInfo = {
        name = "CodeCompanion.nvim with acp-llm-adapter (" .. opts.backend .. " backend)",
        version = "1.0.0",
      },
    },
    handlers = {
      setup = function(_)
        return true
      end,
      auth = function(_)
        return true
      end,
      form_messages = function(self, messages, capabilities)
        return helpers.form_messages(self, messages, capabilities)
      end,
      on_exit = function(_, _) end,
    },
  }
end

local plugins = {
  {
    "olimorris/codecompanion.nvim",
    -- Test with a local CodeCompanion checkout:
    -- dir = "~/code/codecompanion.nvim",
    dependencies = {
      { "nvim-lua/plenary.nvim" },
      {
        "nvim-treesitter/nvim-treesitter",
        lazy = false,
        build = ":TSUpdate",
      },
    },
    opts = {
      adapters = {
        acp = {
          deepseek_acp = function()
            return acp_adapter({
              name = "deepseek_acp",
              formatted_name = "DeepSeek ACP",
              backend = "deepseek",
              api_key_env = "DEEPSEEK_API_KEY",
            })
          end,
          glm_acp = function()
            return acp_adapter({
              name = "glm_acp",
              formatted_name = "GLM ACP",
              backend = "glm",
              api_key_env = "Z_AI_API_KEY",
            })
          end,
          mock_acp = function()
            return acp_adapter({
              name = "mock_acp",
              formatted_name = "Mock ACP",
              backend = "mock",
            })
          end,
        },
      },
      interactions = {
        chat = { adapter = "deepseek_acp" },
      },
      opts = {
        log_level = "DEBUG",
      },
    },
  },
}

require("lazy.minit").repro({ spec = plugins })

require("nvim-treesitter")
  .install({
    "lua",
    "markdown",
    "markdown_inline",
  }, { summary = true, max_jobs = 10 })
  :wait(1800000)
