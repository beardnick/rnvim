-- Remote debugging for rust-analyzer runnables: codelldb runs on the
-- workspace host (next to the binary and the real sources), nvim-dap
-- reaches it through an `ssh -L` tunnel, and a local rewriting proxy
-- translates workspace paths at the DAP protocol boundary — breakpoints
-- go out with mirror paths (ws_root .. <remote path>) and come back as
-- remote ones, exactly like lsp_transport does for LSP. lldb-side source
-- mapping (codelldb's sourceMap) is deliberately NOT used: lldb resolves
-- sources on ITS host, where the original remote paths are the right ones.

local rpc = require("rnvim.rpc")
local util = require("rnvim.util")
local workspace = require("rnvim.workspace")

local M = {}

local TOOLS_PATH = "$HOME/.rnvim/tools/bin:$HOME/.rnvim/tools/npm/node_modules/.bin"
local ADAPTER = "rnvim_codelldb"

-- live tunnel + proxy per dap session, torn down when the session ends
local tunnels = {}

local function is_null(v)
  return v == nil or v == vim.NIL
end

local function free_port()
  local sock = vim.uv.new_tcp()
  sock:bind("127.0.0.1", 0)
  local port = sock:getsockname().port
  sock:close()
  return port
end

--- codelldb present on the workspace host, installing on first use.
local function ensure_codelldb(cb)
  local ok, res = pcall(rpc.request, "exec.which", { name = "codelldb" })
  if ok and not is_null(res.path) then
    return cb(nil)
  end
  util.notify("installing codelldb on the workspace host (first use)...")
  local user_script = require("rnvim.recipes").user_script("codelldb")
  if user_script then
    rpc.request_async("exec.run", { script = user_script }, function(err, rres)
      if err or rres.code ~= 0 then
        cb(err or (rres.stderr:match("([^\n]+)%s*$") or ("exit " .. rres.code)))
      else
        cb(nil)
      end
    end)
    return
  end
  require("rnvim.registry").install("codelldb", function(err)
    cb(err)
  end)
end

--- Build the runnable's binary remotely with cargo; cb(err, remote_exe).
local function build_runnable(runnable, remote_root, cb)
  local args = runnable.args
  local cmd = { "cargo" }
  vim.list_extend(cmd, args.cargoArgs or {})
  vim.list_extend(cmd, args.cargoExtraArgs or {})
  if cmd[2] == "run" then
    cmd[2] = "build"
  else
    cmd[#cmd + 1] = "--no-run"
  end
  cmd[#cmd + 1] = "--message-format=json"

  local quoted = {}
  for _, a in ipairs(cmd) do
    quoted[#quoted + 1] = util.shell_quote(a)
  end
  local script = ("cd %s && %s"):format(util.shell_quote(remote_root), table.concat(quoted, " "))

  util.notify(("building %s on %s..."):format(runnable.label or "runnable", workspace.current().host))
  rpc.request_async("exec.run", { script = script }, function(err, res)
    if err then
      return cb(err)
    end
    if res.code ~= 0 then
      return cb((res.stderr or ""):match("error%[?[^\n]*") or ("cargo exited " .. res.code))
    end
    local exe
    for line in (res.stdout or ""):gmatch("[^\n]+") do
      local ok, msg = pcall(vim.json.decode, line)
      if ok and type(msg) == "table" and not is_null(msg.executable) then
        exe = msg.executable
      end
    end
    if not exe then
      return cb("cargo produced no debuggable executable")
    end
    cb(nil, exe)
  end)
end

--- Start codelldb on `host` behind an ssh -L tunnel; cb(err, port, handle).
local function start_adapter(host, cb)
  local lport = free_port()
  local rport = free_port() -- free here, almost certainly free there
  local remote = ('PATH="%s:$PATH" exec codelldb --port %d'):format(TOOLS_PATH, rport)
  local cmd = {
    "ssh",
    "-o",
    "BatchMode=yes",
    "-o",
    "ExitOnForwardFailure=yes",
    "-L",
    ("%d:127.0.0.1:%d"):format(lport, rport),
    host,
    ('exec "${SHELL:-/bin/sh}" -lc %s'):format(util.shell_quote(remote)),
  }

  local done = false
  local handle
  handle = vim.system(cmd, {}, function(out)
    if not done then
      done = true
      vim.schedule(function()
        cb(("codelldb tunnel exited: %s"):format(vim.trim(out.stderr or "")), nil, nil)
      end)
    end
  end)

  -- Readiness: poll ON THE REMOTE for the adapter port to be listening.
  -- Probing the local end is useless (ssh accepts as soon as it binds,
  -- remote side up or not) and a probe connection can poison codelldb.
  local poll = ([[
for i in $(seq 1 60); do
  if command -v ss >/dev/null 2>&1; then
    ss -ltn 2>/dev/null | grep -q ":%d " && exit 0
  elif command -v netstat >/dev/null 2>&1; then
    netstat -ltn 2>/dev/null | grep -q ":%d " && exit 0
  else
    exit 2
  fi
  sleep 0.5
done
exit 1]]):format(rport, rport)

  rpc.request_async("exec.run", { script = poll }, function(err, res)
    if done then
      return
    end
    done = true
    if err or res.code ~= 0 then
      handle:kill(15)
      local why = err
        or (res.code == 2 and "neither ss nor netstat on the remote host")
        or "timed out waiting for codelldb to listen"
      return cb("codelldb tunnel: " .. why, nil, nil)
    end
    cb(nil, lport, handle)
  end)
end

--- DAP speaks Content-Length framed JSON like LSP: pump frames from `src`
--- to `dst`, deep-rewriting every string value with `xform`.
local function frame_pump(src, dst, xform)
  local rewrite = require("rnvim.lsp_rewrite")
  local buf = ""
  src:read_start(function(err, chunk)
    if err or not chunk then
      if not dst:is_closing() then
        dst:close()
      end
      if not src:is_closing() then
        src:close()
      end
      return
    end
    buf = buf .. chunk
    while true do
      local header_end = buf:find("\r\n\r\n", 1, true)
      if not header_end then
        break
      end
      local len = tonumber(buf:sub(1, header_end):match("Content%-Length:%s*(%d+)"))
      if not len then -- unframed noise: forward raw and resync
        dst:write(buf)
        buf = ""
        break
      end
      local body_start = header_end + 4
      if #buf < body_start - 1 + len then
        break
      end
      local body = buf:sub(body_start, body_start - 1 + len)
      buf = buf:sub(body_start + len)
      local ok, msg = pcall(vim.json.decode, body)
      if ok then
        body = vim.json.encode(rewrite.rewrite(msg, xform))
      end
      dst:write(("Content-Length: %d\r\n\r\n"):format(#body) .. body)
    end
  end)
end

--- Local proxy between nvim-dap and the tunnel that translates workspace
--- paths in both directions. Returns (port, server_handle).
local function start_rewrite_proxy(tunnel_port, ws, remote_root)
  local rewrite = require("rnvim.lsp_rewrite")
  local ws_root = ws.ws_root:gsub("/+$", "")
  local server = assert(vim.uv.new_tcp())
  server:bind("127.0.0.1", 0)
  local port = server:getsockname().port
  server:listen(1, function(err)
    if err then
      return
    end
    local client = vim.uv.new_tcp()
    server:accept(client)
    local upstream = vim.uv.new_tcp()
    upstream:connect("127.0.0.1", tunnel_port, function(cerr)
      if cerr then
        client:close()
        return
      end
      frame_pump(client, upstream, function(s) -- nvim-dap → adapter
        return rewrite.to_remote(ws_root, s)
      end)
      frame_pump(upstream, client, function(s) -- adapter → nvim-dap
        return rewrite.to_local(ws_root, remote_root, s)
      end)
    end)
  end)
  return port, server
end

local function ensure_cleanup_listeners(dap)
  local function drop(session)
    local entry = session and tunnels[session.id]
    if entry then
      tunnels[session.id] = nil
      entry.tunnel:kill(15)
      if not entry.proxy:is_closing() then
        entry.proxy:close()
      end
    end
  end
  dap.listeners.after.event_terminated[ADAPTER] = drop
  dap.listeners.after.event_exited[ADAPTER] = drop
  dap.listeners.after.disconnect[ADAPTER] = drop
end

--- Debug a rust-analyzer runnable (the debugSingle code-lens payload)
--- inside the instance's remote workspace.
function M.debug_rust_runnable(runnable)
  local ok_dap, dap = pcall(require, "dap")
  if not ok_dap then
    util.notify("nvim-dap is not installed", vim.log.levels.WARN)
    return
  end
  local ws = workspace.current()
  if not ws then
    util.notify("no remote workspace attached", vim.log.levels.WARN)
    return
  end
  local args = runnable and runnable.args
  if not args or runnable.kind ~= "cargo" then
    util.notify("unsupported rust-analyzer runnable", vim.log.levels.WARN)
    return
  end

  -- the lens payload crossed the LSP rewrite boundary, so workspaceRoot
  -- arrives as a local ws-prefixed path: translate it back
  local remote_root = args.workspaceRoot or ws.entry
  if vim.startswith(remote_root, ws.ws_root) then
    remote_root = workspace.remote_path(remote_root)
  end

  ensure_codelldb(function(err)
    if err then
      return util.notify("codelldb install failed: " .. err, vim.log.levels.ERROR)
    end
    build_runnable(runnable, remote_root, function(berr, exe)
      if berr then
        return util.notify("build failed: " .. berr, vim.log.levels.ERROR)
      end
      start_adapter(ws.host, function(terr, tunnel_port, handle)
        if terr then
          return util.notify(terr, vim.log.levels.ERROR)
        end
        local proxy_port, proxy = start_rewrite_proxy(tunnel_port, ws, remote_root)
        dap.adapters[ADAPTER] = function(cb)
          cb({ type = "server", host = "127.0.0.1", port = proxy_port })
        end
        ensure_cleanup_listeners(dap)
        dap.run({
          name = runnable.label or "rust debug (remote)",
          type = ADAPTER,
          request = "launch",
          program = exe,
          args = args.executableArgs or {},
          cwd = remote_root,
          stopOnEntry = false,
          -- let codelldb spawn the debuggee itself: its runInTerminal
          -- path would ask nvim-dap to run the (remote) launcher locally
          terminal = "console",
          sourceLanguages = { "rust" },
        })
        local session = dap.session()
        if session and handle then
          tunnels[session.id] = { tunnel = handle, proxy = proxy }
        end
      end)
    end)
  end)
end

return M
