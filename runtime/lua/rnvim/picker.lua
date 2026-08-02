-- fzf-style picker over the remote workspace: fuzzy file finding and live
-- grep. All matching happens on the remote agent (nucleo + ripgrep engine);
-- only the top results cross the wire, so huge repos stay responsive.
--
--   <C-p> / :RnvimFiles   fuzzy file jump
--   <C-g> / :RnvimGrep    live grep
--   <CR> open  <C-n>/<C-p>/<Up>/<Down> move  <C-q> → quickfix  <Esc> close

local rpc = require("rnvim.rpc")

local M = {}
local cfg
local state

local function close()
  if not state then
    return
  end
  local s = state
  state = nil
  if s.timer then
    s.timer:stop()
    s.timer:close()
  end
  pcall(vim.api.nvim_win_close, s.prompt_win, true)
  pcall(vim.api.nvim_win_close, s.list_win, true)
  pcall(vim.api.nvim_buf_delete, s.prompt_buf, { force = true })
  pcall(vim.api.nvim_buf_delete, s.list_buf, { force = true })
  vim.cmd.stopinsert()
end

local function render()
  if not state then
    return
  end
  local lines = {}
  for i, item in ipairs(state.items) do
    lines[i] = " " .. item.display
  end
  if #lines == 0 then
    lines = { "   (no results)" }
  end
  vim.bo[state.list_buf].modifiable = true
  vim.api.nvim_buf_set_lines(state.list_buf, 0, -1, false, lines)
  vim.bo[state.list_buf].modifiable = false
  state.selected = math.max(1, math.min(state.selected, #state.items))
  pcall(vim.api.nvim_win_set_cursor, state.list_win, { state.selected, 0 })
end

local function move(delta)
  if not state or #state.items == 0 then
    return
  end
  state.selected = ((state.selected - 1 + delta) % #state.items) + 1
  pcall(vim.api.nvim_win_set_cursor, state.list_win, { state.selected, 0 })
end

local function search(query)
  if not state then
    return
  end
  state.gen = state.gen + 1
  local gen = state.gen
  if state.mode == "connect" then
    -- Local candidate list; no rpc involved.
    local q = query:lower()
    local items = {}
    for _, item in ipairs(state.all_items) do
      if q == "" or item.display:lower():find(q, 1, true) then
        items[#items + 1] = item
      end
    end
    state.items = items
    state.selected = 1
    render()
    return
  end
  if state.mode == "grep" and query == "" then
    state.items = {}
    render()
    return
  end
  local method = state.mode == "files" and "find.files" or "find.grep"
  rpc.request_async(method, { root = state.root, query = query, limit = 100 }, function(err, res)
    if not state or gen ~= state.gen then
      return
    end
    if err then
      vim.notify("[rnvim] search failed: " .. err, vim.log.levels.ERROR)
      return
    end
    local items = {}
    if state.mode == "files" then
      for _, f in ipairs(res.files or {}) do
        items[#items + 1] = { display = f, path = f }
      end
    else
      for _, m in ipairs(res.matches or {}) do
        items[#items + 1] = {
          display = ("%s:%d: %s"):format(m.path, m.line, m.text),
          path = m.path,
          line = m.line,
          col = m.col,
        }
      end
    end
    state.items = items
    state.selected = 1
    render()
  end)
end

local function current_query()
  local line = vim.api.nvim_buf_get_lines(state.prompt_buf, 0, 1, false)[1] or ""
  return line:sub(#state.prompt_prefix + 1)
end

local function on_change()
  if not state then
    return
  end
  search(current_query())
end

local function target_path(item)
  return cfg.ws_root .. state.root .. "/" .. item.path
end

local function accept()
  if not state then
    return
  end
  if #state.items == 0 then
    close()
    return
  end
  local item = state.items[state.selected]

  if state.mode == "connect" then
    local handoff = M.connect_cfg and M.connect_cfg.handoff
    close()
    if not handoff or handoff == "" then
      vim.notify("[rnvim] no handoff channel (RNVIM_HANDOFF unset)", vim.log.levels.ERROR)
      return
    end
    local f = io.open(handoff, "w")
    if not f then
      vim.notify("[rnvim] cannot write " .. handoff, vim.log.levels.ERROR)
      return
    end
    f:write(item.target)
    f:close()
    local ok = pcall(vim.cmd, "qa")
    if not ok then
      os.remove(handoff)
      vim.notify(
        "[rnvim] unsaved buffers — save them, then :RnvimConnect again",
        vim.log.levels.WARN
      )
    end
    return
  end

  local target = target_path(item)
  close()
  vim.cmd.edit(vim.fn.fnameescape(target))
  if item.line then
    pcall(vim.api.nvim_win_set_cursor, 0, { item.line, math.max((item.col or 1) - 1, 0) })
  end
end

local function to_quickfix()
  if not state or #state.items == 0 then
    return
  end
  local qf = {}
  for _, item in ipairs(state.items) do
    qf[#qf + 1] = {
      filename = target_path(item),
      lnum = item.line or 1,
      col = item.col or 1,
      text = item.display,
    }
  end
  close()
  vim.fn.setqflist(qf, " ")
  vim.cmd.copen()
end

--- Project root on the remote: nearest .git above the session entry,
--- falling back to the entry directory itself. Cached per session.
local function project_root()
  if M._root then
    return M._root
  end
  local entry = vim.env.RNVIM_REMOTE_ENTRY or "/"
  local ok, res = pcall(rpc.request, "fs.findroot", { path = entry, markers = { ".git" } })
  if ok and res.root and res.root ~= vim.NIL then
    M._root = res.root
  else
    local ok_stat, st = pcall(rpc.request, "fs.stat", { path = entry })
    if ok_stat and st.kind == "dir" then
      M._root = entry
    else
      M._root = vim.fs.dirname(entry) or "/"
    end
  end
  return M._root
end

--- Load connect candidates: recent workspaces first, then ssh hosts.
local function connect_items()
  local path = M.connect_cfg and M.connect_cfg.targets
  if not path or path == "" or not vim.uv.fs_stat(path) then
    return {}
  end
  local ok, data = pcall(vim.json.decode, table.concat(vim.fn.readfile(path), "\n"))
  if not ok or type(data) ~= "table" then
    return {}
  end
  local items = {}
  for _, e in ipairs(data.recent or {}) do
    local target = ("%s:%s"):format(e.host, e.path)
    items[#items + 1] = { display = target, target = target }
  end
  for _, h in ipairs(data.hosts or {}) do
    items[#items + 1] = { display = h, target = h }
  end
  return items
end

function M.open(mode)
  if state then
    close()
  end
  local root = mode ~= "connect" and project_root() or nil

  local columns, total_lines = vim.o.columns, vim.o.lines
  local width = math.min(math.floor(columns * 0.8), 120)
  local height = math.max(math.floor(total_lines * 0.5), 5)
  local row = math.max(math.floor((total_lines - height) / 2) - 2, 0)
  local col = math.floor((columns - width) / 2)

  local list_buf = vim.api.nvim_create_buf(false, true)
  local prompt_buf = vim.api.nvim_create_buf(false, true)
  vim.bo[prompt_buf].buftype = "prompt"
  vim.b[list_buf].rnvim_picker = true

  local list_win = vim.api.nvim_open_win(list_buf, false, {
    relative = "editor",
    row = row,
    col = col,
    width = width,
    height = height,
    style = "minimal",
    border = "rounded",
    title = root and (" rnvim %s: %s "):format(mode, root) or (" rnvim %s "):format(mode),
    title_pos = "center",
  })
  local prompt_win = vim.api.nvim_open_win(prompt_buf, true, {
    relative = "editor",
    row = row + height + 2,
    col = col,
    width = width,
    height = 1,
    style = "minimal",
    border = "rounded",
  })
  vim.wo[list_win].cursorline = true

  local prefix = mode .. "> "
  vim.fn.prompt_setprompt(prompt_buf, prefix)

  state = {
    mode = mode,
    root = root,
    items = {},
    all_items = mode == "connect" and connect_items() or nil,
    selected = 1,
    gen = 0,
    timer = vim.uv.new_timer(),
    list_buf = list_buf,
    list_win = list_win,
    prompt_buf = prompt_buf,
    prompt_win = prompt_win,
    prompt_prefix = prefix,
  }

  -- on_lines fires for every change regardless of source (typing, paste,
  -- API) — unlike TextChanged*, which misses programmatic edits.
  vim.api.nvim_buf_attach(prompt_buf, false, {
    on_lines = function()
      if not state then
        return true -- detach
      end
      state.timer:stop()
      state.timer:start(80, 0, vim.schedule_wrap(on_change))
      return false
    end,
  })
  vim.api.nvim_create_autocmd("BufLeave", { buffer = prompt_buf, once = true, callback = close })

  local function imap(lhs, fn)
    vim.keymap.set("i", lhs, fn, { buffer = prompt_buf })
  end
  imap("<CR>", accept)
  imap("<Esc>", close)
  imap("<C-n>", function()
    move(1)
  end)
  imap("<C-p>", function()
    move(-1)
  end)
  imap("<Down>", function()
    move(1)
  end)
  imap("<Up>", function()
    move(-1)
  end)
  imap("<C-q>", to_quickfix)

  vim.cmd.startinsert()
  search("")
end

--- Remote-workspace pickers; requires an active session (rpc).
function M.setup(opts)
  cfg = opts
  vim.api.nvim_create_user_command("RnvimFiles", function()
    M.open("files")
  end, { desc = "rnvim: fuzzy find files" })
  vim.api.nvim_create_user_command("RnvimGrep", function()
    M.open("grep")
  end, { desc = "rnvim: live grep" })
  vim.keymap.set("n", "<C-p>", function()
    M.open("files")
  end, { desc = "rnvim: fuzzy find files" })
  vim.keymap.set("n", "<C-g>", function()
    M.open("grep")
  end, { desc = "rnvim: live grep" })
end

--- Target switcher; works in every mode (local editor included).
function M.setup_connect(opts)
  M.connect_cfg = opts
  vim.api.nvim_create_user_command("RnvimConnect", function()
    M.open("connect")
  end, { desc = "rnvim: connect to a remote target" })
end

return M
