local smoke = dofile("tests/editors/smoke.lua")
smoke.prepend_plugins()
local root = vim.fn.getcwd()

local Session

-- Stand in for the user at LouiseLM's pickers: record what was asked, and
-- answer with `answer` ("Allow once" unless a turn says otherwise).
local asked = {}
local answer = "Allow once"
vim.ui.select = function(items, opts, choose)
  table.insert(asked, opts.prompt)
  for index, item in ipairs(items) do
    local text = opts.format_item and opts.format_item(item) or tostring(item)
    if text:find(answer, 1, true) then
      return choose(item, index)
    end
  end
  choose(nil)
end

local function review_buffer()
  for _, buffer in ipairs(vim.api.nvim_list_bufs()) do
    if vim.api.nvim_buf_is_valid(buffer) and vim.api.nvim_buf_get_name(buffer):match("^louiselm%-diff://") then
      return buffer
    end
  end
end

smoke.run("louiselm", function()
  dofile(root .. "/examples/louiselm.lua")
  local project = smoke.project()
  Session = require("louiselm.session")
  vim.cmd("LouiselmChat")
  local buffer = vim.api.nvim_get_current_buf()
  smoke.wait("adapter session did not become ready", function()
    local sessions = Session.exit_verdict()
    return #sessions == 1 and sessions[1].session:inspect().status == "ready"
  end)
  local session = Session.exit_verdict()[1].session

  local function submit(number, text)
    local lines = vim.api.nvim_buf_get_lines(buffer, 0, -1, false)
    assert(lines[#lines] == "> ", "editable chat prompt is absent before turn " .. number)
    vim.api.nvim_buf_set_lines(buffer, #lines - 1, #lines, false, { "> " .. text })
    smoke.mapping(buffer, "i", "<CR>")()
  end
  local function turn(number, text, expect)
    if text then
      submit(number, text)
    end
    smoke.wait("turn " .. number .. " did not complete with " .. expect, function()
      return session:inspect().current_turn == number
        and session:inspect().status == "ready"
        and smoke.buffer_contains(buffer, expect)
    end)
  end

  turn(1, "Say hello", smoke.reply)

  turn(2, smoke.tool_prompt("run_command", { command = "echo editor-ran" }), smoke.tool_reply)
  assert(#asked >= 1, "the gated command ran without a permission prompt")
  assert(smoke.buffer_contains(buffer, "editor-ran"), "the approved command's output did not come back")

  -- The edit opens LouiseLM's review buffer; read the preview, then accept
  -- with the buffer's own key, as a user would.
  submit(3, smoke.tool_prompt("edit_file", smoke.docstring_edit))
  local review
  smoke.wait("the edit opened no review buffer", function()
    review = review_buffer()
    return review ~= nil
  end)
  smoke.assert_docstring_preview(
    table.concat(vim.api.nvim_buf_get_lines(review, 0, -1, false), "\n"),
    "LouiseLM review buffer"
  )
  smoke.mapping(review, "n", "a")()
  turn(3, nil, smoke.tool_reply)
  local edited = smoke.read(project .. "/hello.py")
  assert(edited:find('"""Greet someone."""', 1, true), "approved edit was not applied")
  assert(edited:find('if __name__ == "__main__":', 1, true), "approved edit lost untouched lines")

  -- A rejected command must not run.
  answer = "Reject once"
  local marker = project .. "/must-not-exist"
  local asked_before = #asked
  submit(4, smoke.tool_prompt("run_command", { command = "touch " .. marker }))
  turn(4, nil, smoke.tool_reply)
  assert(#asked > asked_before, "the rejected command was never put to the user")
  assert(vim.fn.filereadable(marker) == 0, "a rejected command ran")
end, function()
  if Session then
    assert(Session.dispose_all())
  end
end)
