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
