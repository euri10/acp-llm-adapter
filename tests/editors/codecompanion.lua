local smoke = dofile("tests/editors/smoke.lua")
smoke.prepend_plugins()
local root = vim.fn.getcwd()

smoke.run("codecompanion", function()
  dofile(root .. "/examples/codecompanion.lua")
  local project = smoke.project()
  local codecompanion = require("codecompanion")
  codecompanion.chat()
  local chat = assert(codecompanion.last_chat(), "chat did not open")
  local buffer = chat.bufnr
  assert(chat.adapter.name == "deepseek_acp", "chat is not using the snippet adapter")

  local function text()
    return table.concat(vim.api.nvim_buf_get_lines(buffer, 0, -1, false), "\n")
  end
  local function send(prompt)
    -- CodeCompanion locks the buffer until the previous reply finishes.
    smoke.wait("chat buffer never accepted input", function()
      return vim.bo[buffer].modifiable
    end)
    vim.api.nvim_buf_set_lines(buffer, -1, -1, false, { prompt })
    smoke.mapping(buffer, "n", "<CR>")()
  end
  -- Approve the pending request the way a user does, and return what the
  -- chat showed while it waited.
  local function approve(turn, key)
    local shown
    -- Command approvals are titled "Approval Required", edit approvals
    -- "Proposed edits"; both end with the choice list.
    smoke.wait("turn " .. turn .. " never asked for approval", function()
      local current = text()
      local waiting = select(2, current:gsub("Please select an option", "")) >= turn - 1
      if waiting then
        shown = current
      end
      return waiting
    end, function()
      return smoke.tail(buffer, 40)
    end)
    -- g2 is "Accept", g3 "Reject" in CodeCompanion's choice list.
    smoke.mapping(buffer, "n", key or "g2")()
    return shown
  end

  send("Say hello")
  smoke.wait("reply did not reach the chat buffer", function()
    return smoke.buffer_contains(buffer, smoke.reply)
  end)

  send(smoke.tool_prompt("run_command", { command = "echo editor-ran" }))
  approve(2)
  smoke.wait("approved command output did not come back", function()
    return smoke.buffer_contains(buffer, smoke.tool_reply)
      and smoke.buffer_contains(buffer, "editor-ran")
  end, function()
    return smoke.tail(buffer, 40)
  end)

  local replies = select(2, text():gsub(vim.pesc(smoke.tool_reply), ""))
  send(smoke.tool_prompt("edit_file", smoke.docstring_edit))
  local shown = approve(3)
  smoke.assert_docstring_preview(shown, "CodeCompanion approval")
  smoke.wait("approved edit did not complete", function()
    return select(2, text():gsub(vim.pesc(smoke.tool_reply), "")) > replies
  end, function()
    return smoke.tail(buffer, 40)
  end)
  local edited = smoke.read(project .. "/hello.py")
  assert(edited:find('"""Greet someone."""', 1, true), "approved edit was not applied")
  assert(edited:find('if __name__ == "__main__":', 1, true), "approved edit lost untouched lines")

  -- A rejected command must not run.
  local marker = project .. "/must-not-exist"
  replies = select(2, text():gsub(vim.pesc(smoke.tool_reply), ""))
  send(smoke.tool_prompt("run_command", { command = "touch " .. marker }))
  approve(4, "g3")
  smoke.wait("rejected command turn did not complete", function()
    return select(2, text():gsub(vim.pesc(smoke.tool_reply), "")) > replies
  end, function()
    return smoke.tail(buffer, 40)
  end)
  assert(vim.fn.filereadable(marker) == 0, "a rejected command ran")
end, function()
  local chat = require("codecompanion").last_chat()
  if chat then
    chat:close()
  end
end)
