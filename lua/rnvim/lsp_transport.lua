-- LSP transport: run the language server on the workspace host over ssh
-- and rewrite paths in both directions — in pure Lua, at the decoded-
-- message level, by wrapping vim.lsp.rpc. No proxy binary involved.

local rewrite = require("rnvim.lsp_rewrite")
local util = require("rnvim.util")

local M = {}

local TOOLS_PATH = "$HOME/.rnvim/tools/bin:$HOME/.rnvim/tools/npm/node_modules/.bin"

--- The process invocation for a server on `host` ("local" = plain
--- subprocess with the tools prefix on PATH).
local function server_invocation(host, server_cmd)
  if host == "local" then
    return server_cmd, {
      PATH = ("%s/.rnvim/tools/bin:%s/.rnvim/tools/npm/node_modules/.bin:%s"):format(
        vim.uv.os_homedir(),
        vim.uv.os_homedir(),
        vim.env.PATH or ""
      ),
    }
  end
  -- Through the user's login shell so PATH from profile files applies,
  -- with rnvim-installed tools prepended (mirrors exec.which).
  local quoted = {}
  for _, a in ipairs(server_cmd) do
    quoted[#quoted + 1] = util.shell_quote(a)
  end
  local script = ('PATH="%s:$PATH" exec %s'):format(TOOLS_PATH, table.concat(quoted, " "))
  -- Keepalives, for the same reason the agent transport has them: a NAT
  -- or firewall silently dropping the idle connection otherwise leaves a
  -- half-dead server; with them ssh exits within ~90s and the client
  -- reports the death instead of hanging (restart via :RnvimLspRestart).
  return {
    "ssh",
    "-o",
    "BatchMode=yes",
    "-o",
    "ServerAliveInterval=30",
    "-o",
    "ServerAliveCountMax=3",
    host,
    ('exec "${SHELL:-/bin/sh}" -lc %s'):format(util.shell_quote(script)),
  },
    nil
end

--- Build a `cmd` value for vim.lsp.config: a function that starts the
--- server remotely and rewrites every message crossing the boundary.
function M.cmd(host, ws_root, server_cmd)
  ws_root = ws_root:gsub("/+$", "")
  return function(dispatchers)
    local remote_root = nil
    local function outgoing(v) -- nvim → server
      return rewrite.rewrite(v, function(s)
        return rewrite.to_remote(ws_root, s)
      end)
    end
    local function incoming(v) -- server → nvim
      return rewrite.rewrite(v, function(s)
        return rewrite.to_local(ws_root, remote_root, s)
      end)
    end

    local wrapped = {
      notification = function(method, params)
        return dispatchers.notification(method, incoming(params))
      end,
      server_request = function(method, params)
        local result, err = dispatchers.server_request(method, incoming(params))
        return outgoing(result), err
      end,
      on_exit = dispatchers.on_exit,
      on_error = dispatchers.on_error,
    }

    local invocation, env = server_invocation(host, server_cmd)
    local client = vim.lsp.rpc.start(invocation, wrapped, { env = env })
    if not client then
      return nil
    end

    return {
      request = function(method, params, callback, notify_reply_callback)
        local out = outgoing(params)
        if method == "initialize" then
          remote_root = rewrite.capture_remote_root(out)
        end
        return client.request(method, out, function(err, result)
          callback(incoming(err), incoming(result))
        end, notify_reply_callback)
      end,
      notify = function(method, params)
        return client.notify(method, outgoing(params))
      end,
      is_closing = client.is_closing,
      terminate = client.terminate,
    }
  end
end

return M
