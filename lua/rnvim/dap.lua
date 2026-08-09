-- Generic remote debugging engine: a DAP adapter runs on the workspace
-- host (next to the binary and the real sources), nvim-dap reaches it
-- through an `ssh -L` tunnel, and a local rewriting proxy translates
-- workspace paths at the DAP protocol boundary — breakpoints go out with
-- mirror paths (ws_root .. <remote path>) and come back as remote ones,
-- exactly like lsp_transport does for LSP. Adapter-side source mapping
-- (e.g. codelldb's sourceMap) is deliberately NOT used: the adapter
-- resolves sources on ITS host, where the original remote paths are the
-- right ones.
--
-- This module knows nothing about languages or build systems; per-language
-- frontends (rnvim.dap.rust, ...) translate their runnables into a call to
-- M.debug().

local rpc = require("rnvim.rpc")
local util = require("rnvim.util")
local workspace = require("rnvim.workspace")

local M = {}

local TOOLS_PATH = "$HOME/.rnvim/tools/bin:$HOME/.rnvim/tools/npm/node_modules/.bin"
local ADAPTER = "rnvim_dap"

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

--- `bin` present on the workspace host, installing on first use (user
--- recipe first, then the mason-registry planner — same precedence as
--- language servers).
local function ensure_tool(bin, cb)
  local ok, res = pcall(rpc.request, "exec.which", { name = bin })
  if ok and not is_null(res.path) then
    return cb(nil)
  end
  util.notify(("installing %s on the workspace host (first use)..."):format(bin))
  local user_script = require("rnvim.recipes").user_script(bin)
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
  require("rnvim.registry").install(bin, function(err)
    cb(err)
  end)
end

--- Run a build script on the workspace host; cb(err, stdout).
local function run_build(script, label, cb)
  util.notify(("building %s on %s..."):format(label, workspace.current().host))
  rpc.request_async("exec.run", { script = script }, function(err, res)
    if err then
      return cb(err)
    end
    if res.code ~= 0 then
      local stderr = res.stderr or ""
      return cb(stderr:match("[Ee]rror[^\n]*") or stderr:match("([^\n]+)%s*$") or ("build exited " .. res.code))
    end
    cb(nil, res.stdout or "")
  end)
end

--- Start the adapter on `host` behind an ssh -L tunnel; cb(err, port, handle).
--- `adapter` = { command = "codelldb", args = fun(port): string[] }.
local function start_adapter(host, adapter, cb)
  local lport = free_port()
  local rport = free_port() -- free here, almost certainly free there
  local argv = { adapter.command }
  vim.list_extend(argv, adapter.args(rport))
  local quoted = {}
  for _, a in ipairs(argv) do
    quoted[#quoted + 1] = util.shell_quote(a)
  end
  local remote = ('PATH="%s:$PATH" exec %s'):format(TOOLS_PATH, table.concat(quoted, " "))
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
        cb(("%s tunnel exited: %s"):format(adapter.command, vim.trim(out.stderr or "")), nil, nil)
      end)
    end
  end)

  -- Readiness: poll ON THE REMOTE for the adapter port to be listening.
  -- Probing the local end is useless (ssh accepts as soon as it binds,
  -- remote side up or not) and a probe connection can poison the adapter.
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
        or ("timed out waiting for %s to listen"):format(adapter.command)
      return cb(("%s tunnel: %s"):format(adapter.command, why), nil, nil)
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

--- Debug a program on the workspace host.
---
--- opts:
---   name     session name shown by nvim-dap
---   adapter  { command = string, args = fun(port: integer): string[] }
---            a server-mode DAP adapter started on the workspace host;
---            installed on first use like language servers
---   build    optional { script = string, label = string?,
---                       program = (fun(stdout: string): string?)? }
---            remote build step; `program` extracts the debuggee path from
---            the build output and fills config.program when it is unset
---   config   dap launch configuration; program/cwd are REMOTE paths (the
---            adapter interprets them on its own host), source paths cross
---            the boundary through the rewriting proxy
---   root     remote project root used for reverse path mapping (frames
---            under it map back into the local workspace mirror)
function M.debug(opts)
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

  local function launch(program)
    start_adapter(ws.host, opts.adapter, function(terr, tunnel_port, handle)
      if terr then
        return util.notify(terr, vim.log.levels.ERROR)
      end
      local proxy_port, proxy = start_rewrite_proxy(tunnel_port, ws, opts.root)
      dap.adapters[ADAPTER] = function(cb)
        cb({ type = "server", host = "127.0.0.1", port = proxy_port })
      end
      ensure_cleanup_listeners(dap)
      local config = vim.tbl_extend("keep", {
        name = opts.name or "remote debug",
        type = ADAPTER,
        request = "launch",
        program = program,
      }, opts.config or {})
      dap.run(config)
      local session = dap.session()
      if session and handle then
        tunnels[session.id] = { tunnel = handle, proxy = proxy }
      end
    end)
  end

  ensure_tool(opts.adapter.command, function(err)
    if err then
      return util.notify(("%s install failed: %s"):format(opts.adapter.command, err), vim.log.levels.ERROR)
    end
    if not opts.build then
      return launch(nil)
    end
    run_build(opts.build.script, opts.build.label or opts.name or "runnable", function(berr, stdout)
      if berr then
        return util.notify("build failed: " .. berr, vim.log.levels.ERROR)
      end
      local program
      if opts.build.program then
        program = opts.build.program(stdout)
        if not program then
          return util.notify("build produced no debuggable executable", vim.log.levels.ERROR)
        end
      end
      launch(program)
    end)
  end)
end

return M
