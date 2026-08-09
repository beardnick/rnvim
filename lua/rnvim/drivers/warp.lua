-- Warp driver: Warp has no scripting socket, but it registers the warp://
-- URI scheme and can open a Launch Configuration in a new tab. Each spawn
-- writes a one-tab launch config and opens it; focusing an existing tab is
-- not supported. Sessions do not outlive their tab — combine with
-- tmux/screen for persistence.

local util = require("rnvim.util")

local M = {}

local function yaml_quote(s)
  return '"' .. s:gsub("\\", "\\\\"):gsub('"', '\\"') .. '"'
end

function M.spawn(name, target)
  local command = ("RNVIM_TARGET=%s exec nvim --cmd %s"):format(
    util.shell_quote(target),
    util.shell_quote(util.BOOT_CMD)
  )
  local config = ([[---
name: %s
windows:
  - tabs:
      - title: %s
        layout:
          cwd: "~"
          commands:
            - exec: %s
]]):format(yaml_quote("rnvim " .. name), yaml_quote(name), yaml_quote(command))

  local dir = util.ensure_dir(util.home("tmp"))
  local file = vim.fs.joinpath(dir, ("warp-%d.yaml"):format(vim.uv.os_getpid()))
  local f, err = io.open(file, "w")
  if not f then
    return nil, "cannot write warp launch config: " .. tostring(err)
  end
  f:write(config)
  f:close()

  local opener = vim.uv.os_uname().sysname == "Darwin" and "open" or "xdg-open"
  local res = vim.system({ opener, "warp://launch/" .. file }):wait()
  if res.code ~= 0 then
    return nil, (res.stderr or "opening warp:// URI failed"):gsub("%s+$", "")
  end
  return nil -- no handle: warp tabs cannot be focused programmatically
end

function M.focus(_)
  return false, "warp has no remote control — switch to the instance's tab manually"
end

function M.self_handle()
  return nil
end

return M
