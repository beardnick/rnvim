-- Virtual filesystem: maps buffers under the local workspace prefix
-- (~/.rnvim/ws/<host>/<remote abs path>) onto agent fs.* calls.

local rpc = require("rnvim.rpc")

local M = {}
local cfg

--- Strip the workspace prefix, leaving the remote absolute path.
local function to_remote(file)
  local p = file
  if vim.startswith(p, cfg.ws_root) then
    p = p:sub(#cfg.ws_root + 1)
  end
  if p == "" then
    p = "/"
  end
  return p
end

local function set_text(bufnr, text)
  local lines = vim.split(text, "\n", { plain = true })
  local eol = true
  if #lines > 1 and lines[#lines] == "" then
    table.remove(lines)
  elseif text ~= "" then
    eol = false
  end
  vim.api.nvim_buf_set_lines(bufnr, 0, -1, false, lines)
  vim.bo[bufnr].endofline = eol
  vim.bo[bufnr].fixendofline = eol
end

local function open_dir(bufnr, file, remote)
  local res = rpc.request("fs.list", { path = remote })
  local lines = { ("rnvim://%s%s"):format(cfg.host, remote), "" }
  for _, e in ipairs(res.entries or {}) do
    lines[#lines + 1] = e.kind == "dir" and (e.name .. "/") or e.name
  end

  vim.bo[bufnr].modifiable = true
  vim.api.nvim_buf_set_lines(bufnr, 0, -1, false, lines)
  vim.bo[bufnr].modifiable = false
  vim.bo[bufnr].modified = false
  vim.bo[bufnr].buftype = "nowrite"
  vim.bo[bufnr].filetype = "rnvimdir"

  local base = file:gsub("/+$", "")
  vim.keymap.set("n", "<CR>", function()
    local row = vim.api.nvim_win_get_cursor(0)[1]
    if row <= 2 then
      return
    end
    local name = vim.api.nvim_buf_get_lines(bufnr, row - 1, row, false)[1]
    if not name or name == "" then
      return
    end
    vim.cmd.edit(vim.fn.fnameescape(base .. "/" .. name:gsub("/$", "")))
  end, { buffer = bufnr, desc = "rnvim: open entry" })

  vim.keymap.set("n", "-", function()
    local parent = vim.fs.dirname(base)
    if parent and vim.startswith(parent, cfg.ws_root) then
      vim.cmd.edit(vim.fn.fnameescape(parent))
    end
  end, { buffer = bufnr, desc = "rnvim: parent directory" })
end

local function read_buf(bufnr)
  local file = vim.api.nvim_buf_get_name(bufnr)
  local remote = to_remote(file)
  vim.bo[bufnr].swapfile = false

  local st = rpc.request("fs.stat", { path = remote })
  if st.kind == "dir" then
    open_dir(bufnr, file, remote)
    return
  end

  if st.kind == "missing" then
    vim.bo[bufnr].modified = false
    vim.api.nvim_echo({ { ('"%s" [New File] (rnvim: %s)'):format(remote, cfg.host) } }, false, {})
  else
    local res = rpc.request("fs.read", { path = remote })
    set_text(bufnr, vim.base64.decode(res.content_b64))
    vim.bo[bufnr].modified = false
    vim.api.nvim_echo(
      { { ('"%s" %dB (rnvim: %s)'):format(remote, res.size, cfg.host) } },
      false,
      {}
    )
  end

  local ft = vim.filetype.match({ filename = remote, buf = bufnr })
  if ft then
    vim.bo[bufnr].filetype = ft
  end
end

local function write_buf(bufnr)
  local file = vim.api.nvim_buf_get_name(bufnr)
  local remote = to_remote(file)
  local lines = vim.api.nvim_buf_get_lines(bufnr, 0, -1, false)
  local text = table.concat(lines, "\n")
  if vim.bo[bufnr].endofline or vim.bo[bufnr].fixendofline then
    text = text .. "\n"
  end

  local res = rpc.request("fs.write", { path = remote, content_b64 = vim.base64.encode(text) })
  vim.bo[bufnr].modified = false
  vim.api.nvim_echo(
    { { ('"%s" %dL, %dB written (rnvim: %s)'):format(remote, #lines, res.bytes, cfg.host) } },
    false,
    {}
  )
end

function M.setup(opts)
  cfg = opts
  local group = vim.api.nvim_create_augroup("RnvimFs", { clear = true })
  local patterns = { cfg.ws_root, cfg.ws_root .. "/*" }

  vim.api.nvim_create_autocmd("BufReadCmd", {
    group = group,
    pattern = patterns,
    callback = function(ev)
      local ok, err = pcall(read_buf, ev.buf)
      if not ok then
        vim.notify(tostring(err), vim.log.levels.ERROR)
      end
    end,
  })

  vim.api.nvim_create_autocmd("BufWriteCmd", {
    group = group,
    pattern = patterns,
    callback = function(ev)
      local ok, err = pcall(write_buf, ev.buf)
      if not ok then
        vim.notify(tostring(err), vim.log.levels.ERROR)
      end
    end,
  })
end

return M
