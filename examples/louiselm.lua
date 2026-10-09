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
