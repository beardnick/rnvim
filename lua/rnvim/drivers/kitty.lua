-- kitty driver: one workspace per kitty tab, driven over kitty's remote
-- control (requires `allow_remote_control yes` in kitty.conf; kitty then
-- exports KITTY_LISTEN_ON to its windows).

local util = require("rnvim.util")

local M = {}

local function remote_ok()
  return vim.env.KITTY_LISTEN_ON and vim.env.KITTY_LISTEN_ON ~= ""
end

function M.spawn(name, target)
  if not remote_ok() then
    return nil, "kitty remote control is off — set `allow_remote_control yes` in kitty.conf"
  end
  local cmd = {
    "kitty",
    "@",
    "launch",
    "--type=tab",
    "--tab-title",
    name,
    "--env",
    "RNVIM_TARGET=" .. target,
    "nvim",
    "--cmd",
    util.BOOT_CMD,
  }
  local res = vim.system(cmd):wait()
  if res.code ~= 0 then
    return nil, (res.stderr or "kitty @ launch failed"):gsub("%s+$", "")
  end
  return vim.trim(res.stdout or "") -- the new kitty window id
end

function M.focus(handle)
  if not handle or handle == "" then
    return false, "session has no kitty window handle"
  end
  if not remote_ok() then
    return false, "kitty remote control is off — set `allow_remote_control yes` in kitty.conf"
  end
  local res = vim.system({ "kitty", "@", "focus-window", "--match", "id:" .. handle }):wait()
  if res.code ~= 0 then
    return false, (res.stderr or "kitty @ focus-window failed"):gsub("%s+$", "")
  end
  return true
end

function M.self_handle()
  local id = vim.env.KITTY_WINDOW_ID
  return (id and id ~= "") and id or nil
end

return M
