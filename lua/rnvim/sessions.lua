-- Session registry: the single source of truth for which rnvim instances
-- are open. Instances register themselves at connect and deregister on
-- exit; readers sweep entries whose pid is gone (crash, kill -9), so the
-- multiplexer driver never needs a query API — only spawn and focus.

local util = require("rnvim.util")

local M = {}

local function file()
  return util.home("sessions.json")
end

local function alive(pid)
  return pid and vim.uv.kill(pid, 0) == 0
end

--- All live sessions, stale entries swept.
function M.list()
  local data = util.read_json(file()) or {}
  local live = {}
  for _, s in ipairs(data) do
    if alive(s.pid) then
      live[#live + 1] = s
    end
  end
  if #live ~= #data then
    util.write_json(file(), live)
  end
  return live
end

--- Session already open for `target` (host:path), if any.
function M.find(target)
  for _, s in ipairs(M.list()) do
    if s.target == target then
      return s
    end
  end
  return nil
end

--- Register this instance. `handle` is the driver's focus handle
--- (e.g. a tmux window id); nil when spawned outside any multiplexer.
function M.register(target, handle)
  local sessions = M.list()
  sessions[#sessions + 1] = {
    pid = vim.uv.os_getpid(),
    target = target,
    handle = handle,
    driver = require("rnvim.drivers").name(),
  }
  util.write_json(file(), sessions)

  vim.api.nvim_create_autocmd("VimLeavePre", {
    group = vim.api.nvim_create_augroup("RnvimSessions", { clear = true }),
    callback = M.deregister,
  })
end

function M.deregister()
  local me = vim.uv.os_getpid()
  local sessions = {}
  for _, s in ipairs(util.read_json(file()) or {}) do
    if s.pid ~= me then
      sessions[#sessions + 1] = s
    end
  end
  util.write_json(file(), sessions)
end

--- Update this instance's target (directory-selection stage picked a root).
function M.retarget(target)
  local me = vim.uv.os_getpid()
  local sessions = util.read_json(file()) or {}
  for _, s in ipairs(sessions) do
    if s.pid == me then
      s.target = target
    end
  end
  util.write_json(file(), sessions)
end

return M
