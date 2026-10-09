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

function M.wait(what, predicate)
  assert(vim.wait(M.timeout_ms, predicate, 50), what)
end

-- Run `check`, then `cleanup`, and exit nonzero with the first error.
function M.run(name, check, cleanup)
  local ok, err = xpcall(check, debug.traceback)
  local cleanup_ok, cleanup_err = pcall(cleanup)
  if ok and cleanup_ok then
    io.stdout:write(name .. ": reply visible in chat buffer\n")
    vim.cmd("qall!")
  end
  io.stderr:write(name .. ": " .. tostring(ok and cleanup_err or err) .. "\n")
  vim.cmd("cquit 1")
end

return M
