local M = {}

--- Wire the session up. The broker socket always exists (the client runs
--- it even for a bare local editor), so the connect switcher is always
--- available; an initial workspace is registered when the client passed one.
function M.setup()
  local socket = vim.env.RNVIM_SOCKET
  if not socket or socket == "" then
    return
  end

  local workspaces = require("rnvim.workspaces")
  workspaces.setup()

  require("rnvim.rpc").connect(socket)
  require("rnvim.fs").setup()
  require("rnvim.lsp").setup({ rnvim_bin = vim.env.RNVIM_BIN })
  require("rnvim.term").setup()
  require("rnvim.picker").setup({ targets = vim.env.RNVIM_TARGETS })

  local host, ws_root = vim.env.RNVIM_HOST, vim.env.RNVIM_WS_ROOT
  if host and host ~= "" and ws_root and ws_root ~= "" then
    local ws = workspaces.register({
      host = host,
      slug = vim.fs.basename(ws_root),
      ws_root = ws_root,
      abs = vim.env.RNVIM_REMOTE_ENTRY,
    })
    require("rnvim.lsp").register_workspace(ws)
    workspaces.last_active = ws
    vim.t.rnvim_ws = ws.slug

    -- Connected to a host without a path: choose the session root first.
    if vim.env.RNVIM_PENDING_ROOT == "1" then
      vim.schedule(function()
        require("rnvim.picker").open_browse(host, "root")
      end)
    end
  end
end

return M
