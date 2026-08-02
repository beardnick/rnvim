-- :RnvimTerm — a terminal on the remote host, cd'd to the current buffer's
-- remote directory (plain local terminal in loopback sessions).

local M = {}

function M.setup(opts)
  vim.api.nvim_create_user_command("RnvimTerm", function()
    local dir = "~"
    local file = vim.api.nvim_buf_get_name(0)
    if vim.startswith(file, opts.ws_root) then
      local remote = file:sub(#opts.ws_root + 1)
      local st = vim.bo.filetype == "rnvimdir" and remote or vim.fs.dirname(remote)
      if st and st ~= "" then
        dir = st
      end
    end

    vim.cmd.enew()
    if opts.host == "local" then
      vim.fn.jobstart({ vim.o.shell }, { term = true, cwd = dir ~= "~" and dir or nil })
    else
      vim.fn.jobstart({
        "ssh",
        "-t",
        opts.host,
        ("cd %s 2>/dev/null; exec \"${SHELL:-/bin/sh}\" -l"):format(vim.fn.shellescape(dir)),
      }, { term = true })
    end
    vim.cmd.startinsert()
  end, { desc = "rnvim: open a terminal on the remote host" })
end

return M
