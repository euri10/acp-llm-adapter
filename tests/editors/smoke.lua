-- Shared helpers for the editor snippet smoke checks run by
-- scripts/test-editor-snippets. Each check sources a README snippet verbatim
-- from examples/, sends one prompt through the editor's own submit mapping
-- and waits for the fake provider's reply in the chat buffer.
local M = {}

M.reply = assert(vim.env.FAKE_PROVIDER_REPLY, "FAKE_PROVIDER_REPLY is not set")
M.timeout_ms = 30000

function M.prepend_plugins()
  for path in vim.gsplit(assert(vim.env.SNIPPET_PLUGINS, "SNIPPET_PLUGINS is not set"), ":", { plain = true }) do
    vim.opt.rtp:prepend(path)
  end
end

function M.buffer_contains(buffer, text)
  for _, line in ipairs(vim.api.nvim_buf_get_lines(buffer, 0, -1, false)) do
    if line:find(text, 1, true) then
      return true
    end
  end
  return false
end

function M.mapping(buffer, mode, lhs)
  for _, mapping in ipairs(vim.api.nvim_buf_get_keymap(buffer, mode)) do
    if mapping.lhs == lhs and type(mapping.callback) == "function" then
      return mapping.callback
    end
  end
  error(("chat %s-mode %s mapping is absent"):format(mode, lhs))
end

-- Wait for `predicate`; on timeout fail with `what` and, when given, what
-- `context()` reports (e.g. the end of the chat buffer).
function M.wait(what, predicate, context)
  if not vim.wait(M.timeout_ms, predicate, 50) then
    error(what .. (context and ("\n--- context ---\n" .. context()) or ""), 2)
  end
end

function M.tail(buffer, count)
  local lines = vim.api.nvim_buf_get_lines(buffer, 0, -1, false)
  return table.concat(vim.list_slice(lines, math.max(1, #lines - count + 1)), "\n")
end

-- The fake provider answers "!tool <name> <json-args>" with that tool call,
-- then quotes the tool result after this prefix.
M.tool_reply = "tool said: "

function M.tool_prompt(name, args)
  return "!tool " .. name .. " " .. vim.json.encode(args)
end

M.hello = 'def greet(name):\n    return "Hello, " + name + "!"\n\n\nif __name__ == "__main__":\n    print(greet("world"))\n'
M.docstring_edit = {
  path = "hello.py",
  old_text = "def greet(name):\n",
  new_text = 'def greet(name):\n    """Greet someone."""\n',
}

-- A throwaway project with hello.py; Neovim's cwd moves there so sessions
-- and edits never touch the repository. Returns the directory.
function M.project()
  local dir = vim.fn.tempname()
  vim.fn.mkdir(dir, "p")
  local file = assert(io.open(dir .. "/hello.py", "w"))
  file:write(M.hello)
  file:close()
  vim.cmd.cd(dir)
  return dir
end

function M.read(path)
  local file = assert(io.open(path, "r"))
  local text = file:read("*a")
  file:close()
  return text
end

-- What an approved docstring edit must and must not show before approval:
-- the added line, and none of the untouched lines removed.
function M.assert_docstring_preview(text, where)
  assert(text and text:find('+    """Greet someone."""', 1, true), where .. ": preview lacks the added docstring:\n" .. tostring(text))
  assert(not text:find('-if __name__', 1, true), where .. ": preview deletes lines the edit keeps:\n" .. text)
end

-- Run `check`, then `cleanup`, and exit nonzero with the first error.
function M.run(name, check, cleanup)
  local ok, err = xpcall(check, debug.traceback)
  local cleanup_ok, cleanup_err = pcall(cleanup)
  if ok and cleanup_ok then
    io.stdout:write(name .. ": reply, approved and rejected commands, edit preview verified\n")
    vim.cmd("qall!")
  end
  io.stderr:write(name .. ": " .. tostring(ok and cleanup_err or err) .. "\n")
  vim.cmd("cquit 1")
end

return M
