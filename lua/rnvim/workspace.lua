-- The instance's single workspace: this nvim is either purely local (no
-- workspace, connect-picker only) or owns exactly one remote workspace for
-- its whole life. Remote paths live under ~/.rnvim/ws/<slug>/<abs path>.

local util = require("rnvim.util")

local M = {
  ws = nil, -- { host, slug, ws_root, entry, project_root? }
}

function M.base()
  return util.home("ws")
end

--- Adopt `host` + entry path as this instance's workspace.
function M.attach(host, entry)
  local slug = util.host_slug(host)
  M.ws = {
    host = host,
    slug = slug,
    ws_root = vim.fs.joinpath(M.base(), slug),
    entry = entry,
  }
  return M.ws
end

function M.current()
  return M.ws
end

--- The workspace owning `file` (a buffer name), or nil.
function M.of_file(file)
  local ws = M.ws
  if not ws or not file then
    return nil
  end
  if file == ws.ws_root or vim.startswith(file, ws.ws_root .. "/") then
    return ws
  end
  return nil
end

--- Remote absolute path of `file` inside the workspace.
function M.remote_path(file)
  local p = file:sub(#M.ws.ws_root + 1)
  if p == "" then
    p = "/"
  end
  return p
end

--- Local buffer name for a remote absolute path.
function M.local_path(remote)
  return M.ws.ws_root .. remote
end

return M
