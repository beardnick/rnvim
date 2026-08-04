-- Multiplexer drivers. A driver needs only two operations — the session
-- registry (sessions.lua) answers "what is open", so no query API is ever
-- required of the multiplexer:
--
--   spawn(slug, target) → handle?   open a new window running the rnvim
--                                   launcher for `target`; return a focus
--                                   handle or nil
--   focus(handle) → ok, err         bring an existing window to the front
--
-- Detection is by environment; `vim.g.rnvim_driver` overrides.

local M = {}

local function detect()
  local override = vim.g.rnvim_driver
  if type(override) == "string" and override ~= "" then
    return override
  end
  if vim.env.TMUX and vim.env.TMUX ~= "" then
    return "tmux"
  end
  return "none"
end

function M.name()
  return detect()
end

function M.get()
  return require("rnvim.drivers." .. detect())
end

return M
