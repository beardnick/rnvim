-- :RnvimTerm — a terminal on the current workspace's host, cd'd to the
-- current buffer's remote directory (plain local terminal for `local`).

local workspaces = require("rnvim.workspaces")

local M = {}

function M.setup()
  vim.api.nvim_create_user_command("RnvimTerm", function()
    local ws = workspaces.current()
    if not ws then
      vim.notify("[rnvim] no active workspace — :RnvimConnect first", vim.log.levels.WARN)
      return
    end

    local dir = "~"
    local file = vim.api.nvim_buf_get_name(0)
    if workspaces.of_file(file) == ws then
      local remote = workspaces.remote_path(file, ws)
      local d = vim.bo.filetype == "rnvimdir" and remote or vim.fs.dirname(remote)
      if d and d ~= "" then
        dir = d
      end
    end

    vim.cmd.enew()
    if ws.host == "local" then
      vim.fn.jobstart({ vim.o.shell }, { term = true, cwd = dir ~= "~" and dir or nil })
    else
      vim.fn.jobstart({
        "ssh",
        "-t",
        ws.host,
        ("cd %s 2>/dev/null; exec \"${SHELL:-/bin/sh}\" -l"):format(vim.fn.shellescape(dir)),
      }, { term = true })
    end
    vim.cmd.startinsert()
  end, { desc = "rnvim: open a terminal on the workspace host" })
end

return M
