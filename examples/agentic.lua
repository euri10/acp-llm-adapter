-- Load after installing carlos-algms/agentic.nvim; see docs/agentic.md.
require("agentic").setup({
  provider = "deepseek-adapter",
  acp_providers = {
    ["deepseek-adapter"] = {
      name = "DeepSeek (acp-llm-adapter)",
      command = "acp-llm-adapter",
      args = { "serve", "--backend", "deepseek" },
      env = { LLM_API_KEY = assert(os.getenv("DEEPSEEK_API_KEY"), "Set DEEPSEEK_API_KEY before starting Neovim") },
      default_mode = "ask",
    },
  },
})
