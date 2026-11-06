local smoke = dofile("tests/editors/smoke.lua")
smoke.prepend_plugins()
local root = vim.fn.getcwd()
local session
local evidence = assert(vim.env.AGENTIC_EVIDENCE)
local agentic = require("agentic")
local registry = require("agentic.session_registry")

smoke.run("agentic", function()
  dofile(root .. "/examples/agentic.lua")
  local project = smoke.project()
  vim.fn.writefile({ project }, evidence .. "/project-path.txt")
  agentic.open()
  smoke.wait("session did not start", function()
    session = registry.current()
    return session and session.session_id
  end)
  assert(session.provider_name == "deepseek-adapter")
  smoke.wait("ask mode not applied", function()
    return session.config_options:get_mode_id() == "ask"
  end)
  local session_id = session.session_id
  local function chat()
    return table.concat(vim.api.nvim_buf_get_lines(session.widget.buf_nrs.chat, 0, -1, false), "\n")
  end
  local function send(prompt)
    assert(not session.is_generating)
    local input = session.widget.buf_nrs.input
    vim.api.nvim_buf_set_lines(input, 0, -1, false, vim.split(prompt, "\n"))
    smoke.mapping(input, "n", "<CR>")()
    assert(session.is_generating, "submit did not start a turn")
  end
  local function finish()
    smoke.wait("turn did not finish", function()
      return not session.is_generating
    end, chat)
  end
  local function pending()
    smoke.wait("approval absent", function()
      return session.permission_manager:has_pending()
    end, chat)
    return assert(next(session.permission_manager.pending))
  end
  local function choose(kind)
    local id = pending()
    for i, option in ipairs(session.permission_manager.pending[id].sorted_options) do
      if option.kind == kind then
        smoke.mapping(session.widget.buf_nrs.chat, "n", tostring(i))()
        return
      end
    end
    error("missing permission choice " .. kind)
  end
  send("Say hello")
  finish()
  assert(chat():find(smoke.reply, 1, true), "reply absent")
  send(smoke.tool_prompt("read_file", { path = "hello.py" }))
  finish()
  assert(chat():find("Hello,", 1, true), "read result absent")
  send(smoke.tool_prompt("grep", { pattern = "def greet" }))
  finish()
  assert(chat():find("hello.py:1:", 1, true), "search result absent")

  send(smoke.tool_prompt("edit_file", smoke.docstring_edit))
  local id = pending()
  assert(smoke.read(project .. "/hello.py") == smoke.hello, "edit happened before approval")
  local tracker = assert(session.message_writer.tool_call_blocks[id])
  local split = assert(session.diff_coordinator.diff_state.split_state[project .. "/hello.py"], "no visible split diff")
  local before = vim.api.nvim_buf_get_lines(split.original_bufnr, 0, -1, false)
  local after = vim.api.nvim_buf_get_lines(split.new_bufnr, 0, -1, false)
  assert(table.concat(before, "\n") .. "\n" == smoke.hello, "preview old side differs from file")
  local expected = smoke.hello:gsub("def greet%(name%):\n", smoke.docstring_edit.new_text)
  assert(
    table.concat(after, "\n") == expected,
    "preview new side differs from intended edit:\n"
      .. vim.inspect({ before = before, after = after, expected = expected, diff = tracker.diff })
  )
  assert(vim.api.nvim_win_get_buf(split.new_winid) == split.new_bufnr, "preview is not displayed")
  vim.fn.writefile(after, evidence .. "/approval-new.py")
  assert(tracker.diff, "ACP diff absent")
  choose("allow_once")
  finish()
  assert(smoke.read(project .. "/hello.py"):find('"""Greet someone."""', 1, true))

  send(smoke.tool_prompt("write_file", { path = "denied.txt", content = "must not be written" }))
  choose("reject_once")
  finish()
  assert(vim.fn.filereadable(project .. "/denied.txt") == 0, "denied write executed")

  send(smoke.tool_prompt("run_command", { command = "touch started; sleep 3; touch cancelled-too-late" }))
  choose("allow_once")
  smoke.wait("command never started", function()
    return vim.fn.filereadable(project .. "/started") == 1
  end)
  agentic.stop_generation()
  finish()
  assert(not vim.wait(3500, function()
    return vim.fn.filereadable(project .. "/cancelled-too-late") == 1
  end, 50), "cancelled command performed its delayed write")
  send("Continue after cancellation")
  finish()
  assert(session.session_id == session_id, "continuation changed session identity")
  agentic.close()
  agentic.open()
  assert(registry.current() == session, "hide/show lost session")
  send("Continue after hiding the panel")
  finish()
  -- Drive the real keymap/selector callbacks, choosing from the offered values.
  local function select_option(key, value)
    local old_select = vim.ui.select
    local chosen
    vim.ui.select = function(items, _, callback)
      for _, item in ipairs(items) do
        if (value and item.value == value) or (not value and item.value ~= session.config_options:get_model_id()) then
          chosen = item.value
          callback(item)
          return
        end
      end
      error("selector did not offer requested value")
    end
    smoke.mapping(session.widget.buf_nrs.input, "n", key)()
    vim.ui.select = old_select
    assert(chosen, "selector did not open")
    return chosen
  end
  select_option("<S-Tab>", "plan")
  smoke.wait("plan mode not applied", function()
    return session.config_options:get_mode_id() == "plan"
  end)
  select_option("<S-Tab>", "ask")
  smoke.wait("ask mode not restored", function()
    return session.config_options:get_mode_id() == "ask"
  end)
  local model = select_option("\\m")
  smoke.wait("model change not applied", function()
    return session.config_options:get_model_id() == model
  end)
  send("!model")
  finish()
  assert(chat():find("request model: " .. model, 1, true), "selected model did not reach provider")
  -- Destroy the host widget, then exercise the public restore picker and ACP load.
  agentic.destroy_session()
  local old_select = vim.ui.select
  vim.ui.select = function(items, _, callback)
    for _, item in ipairs(items) do
      if item.session_id == session_id then
        callback(item)
        return
      end
    end
    error("saved session absent from restore picker")
  end
  agentic.restore_session()
  smoke.wait("session was not restored", function()
    local restored = registry.current()
    if restored and restored ~= session and restored.session_id == session_id then
      session = restored
      return true
    end
  end)
  vim.ui.select = old_select
  assert(chat():find("Continue after cancellation", 1, true), "restored transcript lacks previous prompt")
  assert(session.config_options:get_model_id() == model, "restored model differs")
  send("!history Continue after cancellation")
  finish()
  assert(
    chat():find("history retained: Continue after cancellation", 1, true),
    "provider request lost the restored history"
  )
  send("!history absent-from-earlier-turn-unique")
  finish()
  assert(
    chat():find("history missing: absent-from-earlier-turn-unique", 1, true),
    "history check counted its own prompt"
  )
  vim.fn.writefile(vim.split(chat(), "\n"), evidence .. "/chat.txt")
  io.stdout:write(
    "agentic: read/search/edit, preview, denied write, cancellation, mode/model controls and restored continuation passed\n"
  )
end, function()
  require("agentic.acp.agent_instance"):cleanup_all()
end)
