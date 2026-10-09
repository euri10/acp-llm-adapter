local smoke = dofile("tests/editors/smoke.lua")
smoke.prepend_plugins()

smoke.run("codecompanion", function()
  dofile("examples/codecompanion.lua")
  local codecompanion = require("codecompanion")
  codecompanion.chat()
  local chat = assert(codecompanion.last_chat(), "chat did not open")
  local buffer = chat.bufnr
  assert(chat.adapter.name == "deepseek_acp", "chat is not using the snippet adapter")
  vim.api.nvim_buf_set_lines(buffer, -1, -1, false, { "Say hello" })
  smoke.mapping(buffer, "n", "<CR>")()
  smoke.wait("reply did not reach the chat buffer", function()
    return smoke.buffer_contains(buffer, smoke.reply)
  end)
end, function()
  local chat = require("codecompanion").last_chat()
  if chat then
    chat:close()
  end
end)
