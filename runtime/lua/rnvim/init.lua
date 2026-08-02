local M = {}

--- Wire the remote session up if the client launched us with one.
--- Without RNVIM_SOCKET this is a plain local editor: only the target
--- switcher (:RnvimConnect) is wired up.
function M.setup()
  require("rnvim.picker").setup_connect({
    targets = vim.env.RNVIM_TARGETS,
    handoff = vim.env.RNVIM_HANDOFF,
  })

  local socket = vim.env.RNVIM_SOCKET
  if not socket or socket == "" then
    return
  end

  local ws_root = vim.env.RNVIM_WS_ROOT
  if not ws_root or ws_root == "" then
    vim.notify("[rnvim] RNVIM_SOCKET set but RNVIM_WS_ROOT missing", vim.log.levels.ERROR)
    return
  end

  local opts = {
    ws_root = ws_root:gsub("/+$", ""),
    host = vim.env.RNVIM_HOST or "remote",
    rnvim_bin = vim.env.RNVIM_BIN,
  }

  require("rnvim.rpc").connect(socket)
  require("rnvim.fs").setup(opts)
  require("rnvim.lsp").setup(opts)
  require("rnvim.term").setup(opts)
  require("rnvim.picker").setup(opts)
end

return M
