-- Ghostty driver: Ghostty has no remote-control API, so a new workspace
-- opens as a new OS window (`ghostty -e`; on macOS via `open -na`) and
-- focusing an existing one is up to the window manager. Sessions do not
-- outlive their window — combine with tmux/screen for persistence.

local util = require("rnvim.util")

local M = {}

function M.spawn(name, target)
  local _ = name -- Ghostty windows cannot be titled from the CLI
  local argv = util.instance_argv(target)
  local cmd
  if vim.uv.os_uname().sysname == "Darwin" then
    cmd = { "open", "-na", "Ghostty.app", "--args", "-e" }
  else
    cmd = { "ghostty", "-e" }
  end
  vim.list_extend(cmd, argv)
  local res = vim.system(cmd, { detach = true }):wait()
  if res.code ~= 0 then
    return nil, (res.stderr or "launching ghostty failed"):gsub("%s+$", "")
  end
  return nil -- no handle: ghostty windows cannot be focused programmatically
end

function M.focus(_)
  return false, "ghostty has no remote control — switch to the instance's window manually"
end

function M.self_handle()
  return nil
end

return M
