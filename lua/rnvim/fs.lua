-- Virtual filesystem: buffers under ~/.rnvim/ws/<slug>/<remote abs path>
-- map onto agent fs.* calls.

local rpc = require("rnvim.rpc")
local workspace = require("rnvim.workspace")

local M = {}

local function buf_remote(bufnr)
  local file = vim.api.nvim_buf_get_name(bufnr)
  local ws = workspace.of_file(file)
  if not ws then
    error(("[rnvim] %s is under the workspace prefix but this instance is not connected for it"):format(file))
  end
  return ws, workspace.remote_path(file)
end

local function short(path)
  if #path > 50 then
    return "..." .. path:sub(-49)
  end
  return path
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

local function open_dir(bufnr, file, ws, remote)
  local res = rpc.request("fs.list", { path = remote })
  local lines = { ("rnvim://%s%s"):format(ws.host, remote), "" }
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
    if parent and vim.startswith(parent, ws.ws_root) then
      vim.cmd.edit(vim.fn.fnameescape(parent))
    end
  end, { buffer = bufnr, desc = "rnvim: parent directory" })
end

local function read_buf(bufnr)
  local ws, remote = buf_remote(bufnr)
  local file = vim.api.nvim_buf_get_name(bufnr)
  vim.bo[bufnr].swapfile = false

  local st = rpc.request("fs.stat", { path = remote })
  if st.kind == "dir" then
    open_dir(bufnr, file, ws, remote)
    return
  end

  if st.kind == "missing" then
    vim.bo[bufnr].modified = false
    vim.api.nvim_echo({ { ('"%s" [New File] (rnvim: %s)'):format(short(remote), ws.host) } }, false, {})
  else
    local res = rpc.request("fs.read", { path = remote })
    set_text(bufnr, vim.base64.decode(res.content_b64))
    vim.bo[bufnr].modified = false
    vim.api.nvim_echo({ { ('"%s" %dB (rnvim: %s)'):format(short(remote), res.size, ws.host) } }, false, {})
  end

  -- BufReadCmd suppresses BufReadPost the same way; firing it lets the
  -- stock filetype detection and any user read hooks treat this like a
  -- normal file load.
  vim.api.nvim_exec_autocmds("BufReadPost", { buffer = bufnr, modeline = false })
  if vim.bo[bufnr].filetype == "" then
    local ft = vim.filetype.match({ filename = remote, buf = bufnr })
    if ft then
      vim.bo[bufnr].filetype = ft
    end
  end
end

local function write_buf(bufnr)
  local ws, remote = buf_remote(bufnr)

  -- BufWriteCmd REPLACES the whole write, so the Pre/Post write events
  -- never fire on their own. They carry real machinery: LSP didSave
  -- (rust-analyzer only re-runs cargo check on save), format-on-save
  -- hooks, code-lens refreshers. Fire them around the virtual write.
  vim.api.nvim_exec_autocmds("BufWritePre", { buffer = bufnr, modeline = false })

  local lines = vim.api.nvim_buf_get_lines(bufnr, 0, -1, false)
  local text = table.concat(lines, "\n")
  if vim.bo[bufnr].endofline or vim.bo[bufnr].fixendofline then
    text = text .. "\n"
  end

  local res = rpc.request("fs.write", { path = remote, content_b64 = vim.base64.encode(text) })
  vim.bo[bufnr].modified = false
  vim.api.nvim_echo(
    { { ('"%s" %dL, %dB written (rnvim: %s)'):format(short(remote), #lines, res.bytes, ws.host) } },
    false,
    {}
  )

  vim.api.nvim_exec_autocmds("BufWritePost", { buffer = bufnr, modeline = false })
end

function M.setup()
  local group = vim.api.nvim_create_augroup("RnvimFs", { clear = true })
  local patterns = { workspace.base() .. "/*" }

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
