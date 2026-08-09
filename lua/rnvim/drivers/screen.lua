-- GNU screen driver: one workspace per screen window. Sticks to commands
-- available in screen 4.0 (macOS ships 4.00 from 2006: no -Q query
-- support), so the focus handle is the window title and self-identity
-- comes from the $WINDOW variable screen exports to its windows.

local util = require("rnvim.util")

local M = {}

function M.spawn(name, target)
  local cmd = { "screen", "-X", "screen", "-t", name }
  vim.list_extend(cmd, util.instance_argv(target))
  local res = vim.system(cmd):wait()
  if res.code ~= 0 then
    return nil, (res.stderr or "screen -X screen failed"):gsub("%s+$", "")
  end
  return name -- select matches by title
end

function M.focus(handle)
  if not handle or handle == "" then
    return false, "session has no screen window handle"
  end
  local res = vim.system({ "screen", "-X", "select", handle }):wait()
  if res.code ~= 0 then
    return false, (res.stderr or "screen -X select failed"):gsub("%s+$", "")
  end
  return true
end

function M.self_handle()
  local w = vim.env.WINDOW
  return (w and w ~= "") and w or nil
end

return M
