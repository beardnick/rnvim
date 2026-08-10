-- fzf-style picker.
--
-- In a remote instance (vim.g.rnvim set):
--   <C-p> / :RnvimFiles     fuzzy file jump (matching runs on the agent)
--   <C-g> / :RnvimGrep      live grep
--   browse mode             directory-selection stage for a bare-host
--                           connect (<CR> descend · <C-s> pick as root)
--
-- Everywhere (local instances too):
--   :RnvimConnect           pick a target; open sessions switch via the
--                           multiplexer driver, new targets spawn a fresh
--                           instance in a new window

local M = {}
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

local function retitle(title)
  if state and state.list_win then
    pcall(vim.api.nvim_win_set_config, state.list_win, { title = title, title_pos = "center" })
  end
end

local function clear_query()
  if not state then
    return
  end
  vim.api.nvim_buf_set_lines(state.prompt_buf, 0, -1, false, { state.prompt_prefix })
  pcall(vim.api.nvim_win_set_cursor, state.prompt_win, { 1, #state.prompt_prefix })
end

--- Fetch a remote directory for browse mode (remote instance only —
--- this is the directory-selection stage before the workspace roots).
local function fetch_browse(path)
  if not state then
    return
  end
  require("rnvim.rpc").request_async("fs.resolve", { path = path }, function(err, res)
    if not state or state.mode ~= "browse" then
      return
    end
    if err then
      vim.notify("[rnvim] browse failed: " .. err, vim.log.levels.ERROR)
      return
    end
    require("rnvim.rpc").request_async("fs.list", { path = res.abs }, function(lerr, lres)
      if not state or state.mode ~= "browse" then
        return
      end
      if lerr then
        vim.notify("[rnvim] browse failed: " .. lerr, vim.log.levels.ERROR)
        return
      end
      state.browse_path = res.abs
      state.browse_entries = lres.entries or {}
      retitle((" rnvim browse %s:%s · <CR> enter · <C-s> choose this dir "):format(state.browse_host, res.abs))
      clear_query()
      M._research()
    end)
  end)
end

local function search(query)
  if not state then
    return
  end
  state.gen = state.gen + 1
  local gen = state.gen
  if state.mode == "browse" then
    local q = query:lower()
    local items = {}
    if state.browse_path and state.browse_path ~= "/" then
      items[#items + 1] = { display = "../", up = true }
    end
    for _, e in ipairs(state.browse_entries or {}) do
      if e.kind == "dir" and (q == "" or e.name:lower():find(q, 1, true)) then
        items[#items + 1] = { display = e.name .. "/", name = e.name }
      end
    end
    state.items = items
    state.selected = 1
    render()
    return
  end
  if state.mode == "connect" then
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
  require("rnvim.rpc").request_async(method, { root = state.root, query = query, limit = 100 }, function(err, res)
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
  return state.ws.ws_root .. state.root .. "/" .. item.path
end

--- Open (or switch to) `target`. Open session → driver focus; new target
--- → driver spawn. `host` with no path spawns straight away — the new
--- instance runs the directory-selection stage itself.
local function open_target(target)
  local sessions = require("rnvim.sessions")
  local drivers = require("rnvim.drivers")
  local util = require("rnvim.util")

  local existing = sessions.find(target)
  if existing then
    local ok, err = drivers.get().focus(existing.handle)
    if not ok then
      vim.notify("[rnvim] " .. (err or "cannot switch"), vim.log.levels.WARN)
    end
    return
  end

  local handle, err = drivers.get().spawn(util.window_name(target), target)
  if err then
    vim.notify("[rnvim] " .. err, vim.log.levels.WARN)
  elseif handle ~= nil then
    vim.notify("[rnvim] opened " .. target)
  end
end

local function accept()
  if not state then
    return
  end
  if #state.items == 0 then
    if state.mode == "connect" then
      -- nothing matched: treat the typed query as a target itself
      -- (an ad-hoc user@host or host:path needs no ssh-config entry)
      local q = vim.trim(current_query())
      close()
      if q ~= "" then
        open_target(q)
      end
      return
    end
    close()
    return
  end
  local item = state.items[state.selected]

  if state.mode == "browse" then
    -- <CR> descends; <C-s> chooses the current directory.
    local next_path
    if item.up then
      next_path = vim.fs.dirname(state.browse_path)
    else
      next_path = state.browse_path:gsub("/+$", "") .. "/" .. item.name
    end
    fetch_browse(next_path)
    return
  end

  if state.mode == "connect" then
    local target = item.target
    close()
    open_target(target)
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

--- <C-s> in browse mode: the browsed directory becomes this instance's
--- workspace root.
local function choose_dir()
  if not state or state.mode ~= "browse" then
    return
  end
  local path = state.browse_path
  if not path then
    return
  end
  local on_rooted = state.browse_on_rooted
  close()
  if on_rooted then
    on_rooted(path)
  end
end

--- Project root on the workspace host: nearest .git above its entry,
--- falling back to the entry directory. Cached on the workspace.
local function project_root(ws)
  if ws.project_root then
    return ws.project_root
  end
  local rpc = require("rnvim.rpc")
  local entry = ws.entry or "/"
  local ok, res = pcall(rpc.request, "fs.findroot", { path = entry, markers = { ".git" } })
  if ok and res.root and res.root ~= vim.NIL then
    ws.project_root = res.root
  else
    local ok_stat, st = pcall(rpc.request, "fs.stat", { path = entry })
    if ok_stat and st.kind == "dir" then
      ws.project_root = entry
    else
      ws.project_root = vim.fs.dirname(entry) or "/"
    end
  end
  return ws.project_root
end

--- Connect candidates: open sessions first, then recents, then ssh
--- hosts. EVERY ssh host is listed bare — even one whose recents appear
--- above — because the bare entry is how you open a NEW directory on
--- that host (it runs the directory-selection stage in the new instance).
local function connect_items()
  local items = {}
  local me = vim.uv.os_getpid()
  for _, s in ipairs(require("rnvim.sessions").list()) do
    if s.pid ~= me then
      items[#items + 1] = { display = ("● %s  [open]"):format(s.target), target = s.target }
    end
  end
  local remotes = require("rnvim.remotes")
  for _, e in ipairs(remotes.load_recent()) do
    local target = ("%s:%s"):format(e.host, e.path)
    items[#items + 1] = { display = target, target = target }
  end
  for _, h in ipairs(remotes.ssh_hosts()) do
    items[#items + 1] = { display = h, target = h }
  end
  return items
end

function M.open(mode)
  if state then
    close()
  end

  local ws, root
  if mode ~= "connect" and mode ~= "browse" then
    ws = require("rnvim.workspace").current()
    if not ws then
      vim.notify("[rnvim] this instance has no workspace — :RnvimConnect opens one", vim.log.levels.WARN)
      return
    end
    root = project_root(ws)
  end

  local columns, total_lines = vim.o.columns, vim.o.lines
  local width = math.min(math.floor(columns * 0.8), 120)
  local height = math.max(math.floor(total_lines * 0.5), 5)
  local row = math.max(math.floor((total_lines - height) / 2) - 2, 0)
  local col = math.floor((columns - width) / 2)

  local list_buf = vim.api.nvim_create_buf(false, true)
  local prompt_buf = vim.api.nvim_create_buf(false, true)
  vim.bo[prompt_buf].buftype = "prompt"
  vim.b[list_buf].rnvim_picker = true

  local title
  if mode == "connect" then
    title = " rnvim connect · <CR> select "
  elseif mode == "browse" then
    title = " rnvim browse "
  else
    title = (" rnvim %s: %s:%s "):format(mode, ws.host, root)
  end

  local list_win = vim.api.nvim_open_win(list_buf, false, {
    relative = "editor",
    row = row,
    col = col,
    width = width,
    height = height,
    style = "minimal",
    border = "rounded",
    title = title,
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
    ws = ws,
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
  imap("<C-s>", choose_dir)

  vim.cmd.startinsert()
  if mode ~= "browse" then
    search("")
  end
end

--- Directory-selection stage (remote instance, bare-host connect): browse
--- the host and pick a directory; `on_rooted(path)` adopts it.
function M.open_browse(host, on_rooted)
  M.open("browse")
  if not state then
    return
  end
  state.browse_host = host
  state.browse_on_rooted = on_rooted
  fetch_browse("~")
end

-- exposed for tests
M._connect_items = connect_items

--- Re-run the current search (used by async browse fetches).
function M._research()
  if state then
    search(current_query())
  end
end

--- Commands available everywhere; workspace pickers only bind where a
--- workspace exists.
function M.setup(opts)
  vim.api.nvim_create_user_command("RnvimConnect", function()
    M.open("connect")
  end, { desc = "rnvim: open or switch to a remote workspace" })

  if opts and opts.workspace then
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
end

return M
