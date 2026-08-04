-- :RnvimTerm — a terminal on the workspace host, cd'd to the current
-- buffer's remote directory (plain local terminal for `local:`).

local workspace = require("rnvim.workspace")

local M = {}

function M.setup()
  vim.api.nvim_create_user_command("RnvimTerm", function()
    local ws = workspace.current()
    if not ws then
      vim.notify("[rnvim] this instance has no workspace", vim.log.levels.WARN)
      return
    end

    local dir = "~"
    local file = vim.api.nvim_buf_get_name(0)
    if workspace.of_file(file) then
      local remote = workspace.remote_path(file)
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
        ('cd %s 2>/dev/null; exec "${SHELL:-/bin/sh}" -l'):format(vim.fn.shellescape(dir)),
      }, { term = true })
    end
    vim.cmd.startinsert()
  end, { desc = "rnvim: open a terminal on the workspace host" })
end

return M
