local smoke = dofile("tests/editors/smoke.lua")
smoke.prepend_plugins()

local Session

smoke.run("louiselm", function()
  dofile("examples/louiselm.lua")
  Session = require("louiselm.session")
  vim.cmd("LouiselmChat")
  local buffer = vim.api.nvim_get_current_buf()
  smoke.wait("adapter session did not become ready", function()
    local sessions = Session.exit_verdict()
    return #sessions == 1 and sessions[1].session:inspect().status == "ready"
  end)
  local session = Session.exit_verdict()[1].session
  local lines = vim.api.nvim_buf_get_lines(buffer, 0, -1, false)
  assert(lines[#lines] == "> ", "editable chat prompt is absent")
  vim.api.nvim_buf_set_lines(buffer, #lines - 1, #lines, false, { "> Say hello" })
  smoke.mapping(buffer, "i", "<CR>")()
  smoke.wait("reply did not reach the chat buffer", function()
    return session:inspect().current_turn == 1
      and session:inspect().status == "ready"
      and smoke.buffer_contains(buffer, smoke.reply)
  end)
end, function()
  if Session then
    assert(Session.dispose_all())
  end
end)
