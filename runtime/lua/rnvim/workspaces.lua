-- Workspace registry: every connected remote maps to a prefix directory
-- ~/.rnvim/ws/<slug>/. A buffer belongs to the workspace whose prefix it
-- lives under; that decides where its rpc traffic routes.

local M = {
  by_slug = {},
  by_host = {},
  last_active = nil,
}

local ws_base = ""

function M.setup()
  ws_base = (vim.env.RNVIM_WS_BASE or ""):gsub("/+$", "")

  -- Track the most recently touched workspace, so pickers opened from
  -- non-workspace buffers (help, empty tab, ...) still have a context.
  vim.api.nvim_create_autocmd("BufEnter", {
    group = vim.api.nvim_create_augroup("RnvimWorkspaces", { clear = true }),
    callback = function(ev)
      local ws = M.of_file(vim.api.nvim_buf_get_name(ev.buf))
      if ws then
        M.last_active = ws
      end
    end,
  })
end

function M.base()
  return ws_base
end

--- Register (or fetch) a workspace. info: {host, slug, ws_root, abs}.
function M.register(info)
  local ws = M.by_slug[info.slug]
  if not ws then
    ws = {
      host = info.host,
      slug = info.slug,
      ws_root = info.ws_root:gsub("/+$", ""),
      entry = info.abs,
    }
    M.by_slug[info.slug] = ws
    M.by_host[info.host] = ws
  end
  return ws
end

--- The workspace owning `file` (a buffer name), or nil.
function M.of_file(file)
  if ws_base == "" or not file or not vim.startswith(file, ws_base .. "/") then
    return nil
  end
  local slug = file:sub(#ws_base + 2):match("^([^/]+)")
  return slug and M.by_slug[slug] or nil
end

--- Remote absolute path of `file` inside workspace `ws`.
function M.remote_path(file, ws)
  local p = file:sub(#ws.ws_root + 1)
  if p == "" then
    p = "/"
  end
  return p
end

--- Workspace for the current buffer, falling back to the last active one.
function M.current()
  return M.of_file(vim.api.nvim_buf_get_name(0)) or M.last_active
end

return M
