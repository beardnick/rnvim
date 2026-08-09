-- tmux driver: one workspace per tmux window.

local util = require("rnvim.util")

local M = {}

function M.spawn(name, target)
  local res = vim
    .system({
      "tmux",
      "new-window",
      "-n",
      name,
      "-P",
      "-F",
      "#{window_id}",
      "-e",
      "RNVIM_TARGET=" .. target,
      ("nvim --cmd %s"):format(util.shell_quote(util.BOOT_CMD)),
    })
    :wait()
  if res.code ~= 0 then
    return nil, (res.stderr or "tmux new-window failed"):gsub("%s+$", "")
  end
  return vim.trim(res.stdout or "")
end

function M.focus(handle)
  if not handle or handle == "" then
    return false, "session has no tmux window handle"
  end
  local res = vim.system({ "tmux", "select-window", "-t", handle }):wait()
  if res.code ~= 0 then
    return false, (res.stderr or "tmux select-window failed"):gsub("%s+$", "")
  end
  return true
end

--- Handle of the window this instance runs in (registered at connect so
--- other instances can focus us).
function M.self_handle()
  local res = vim.system({ "tmux", "display-message", "-p", "#{window_id}" }):wait()
  if res.code ~= 0 then
    return nil
  end
  return vim.trim(res.stdout or "")
end

return M
