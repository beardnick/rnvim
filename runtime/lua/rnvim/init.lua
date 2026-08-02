local M = {}

--- Wire the remote session up if the client launched us with one.
--- Without RNVIM_SOCKET this is a plain local editor and setup is a no-op.
function M.setup()
  local socket = vim.env.RNVIM_SOCKET
  if not socket or socket == "" then
    return
  end

  local ws_root = vim.env.RNVIM_WS_ROOT
  if not ws_root or ws_root == "" then
    vim.notify("[rnvim] RNVIM_SOCKET set but RNVIM_WS_ROOT missing", vim.log.levels.ERROR)
    return
  end

  require("rnvim.rpc").connect(socket)
  require("rnvim.fs").setup({
    ws_root = ws_root:gsub("/+$", ""),
    host = vim.env.RNVIM_HOST or "remote",
  })
end

return M
