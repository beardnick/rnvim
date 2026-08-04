-- JSON-lines RPC to the workspace agent, carried by the agent process's
-- stdio (ssh for remote hosts, a plain subprocess for `local:`).
--
-- One instance owns exactly one workspace, so there is exactly one agent
-- and no routing. Requests are synchronous from the caller's point of view
-- via vim.wait() (which keeps pumping the uv loop); interactive UIs use
-- request_async.

local util = require("rnvim.util")

local M = {
  pending = {},
  next_id = 1,
}

local job -- vim.system handle
local stdin -- uv pipe for writes
local rxbuf = ""

local function on_line(line)
  local ok, msg = pcall(vim.json.decode, line)
  if not ok or type(msg) ~= "table" or not msg.id then
    return
  end
  local entry = M.pending[msg.id]
  if not entry then
    return
  end
  if entry.cb then
    M.pending[msg.id] = nil
    vim.schedule(function()
      local err = msg.error and (msg.error.message or "remote error") or nil
      entry.cb(err, msg.result)
    end)
  else
    entry.msg = msg
    entry.done = true
  end
end

local function on_stdout(_, data)
  if not data then
    return
  end
  rxbuf = rxbuf .. data
  while true do
    local nl = rxbuf:find("\n", 1, true)
    if not nl then
      break
    end
    local line = rxbuf:sub(1, nl - 1)
    rxbuf = rxbuf:sub(nl + 1)
    if line ~= "" then
      on_line(line)
    end
  end
end

--- The command line that runs the agent for `host`.
local function agent_cmd(host)
  if host == "local" then
    local bin = vim.env.RNVIM_AGENT_BIN or (vim.g.rnvim_agent_bin --[[@as string?]])
    if not bin or bin == "" then
      bin = require("rnvim.deploy").local_agent_bin()
    end
    return { bin, "--stdio" }
  end
  local remote_cmd = require("rnvim.deploy").ensure_remote_agent(host)
  return {
    "ssh",
    "-o",
    "BatchMode=yes",
    "-o",
    "ServerAliveInterval=30",
    "-o",
    "ServerAliveCountMax=3",
    host,
    remote_cmd,
  }
end

function M.connected()
  return job ~= nil
end

--- Spawn the agent and complete the hello handshake. Errors loudly.
function M.connect(host)
  if job then
    return
  end
  local cmd = agent_cmd(host)
  job = vim.system(cmd, {
    stdin = true,
    stdout = on_stdout,
    stderr = function(_, data)
      if data and data ~= "" then
        -- ssh banners/warnings: surface but don't die
        vim.schedule(function()
          vim.notify("[rnvim agent] " .. data:gsub("%s+$", ""), vim.log.levels.DEBUG)
        end)
      end
    end,
  }, function(out)
    local dead_job = job
    job = nil
    stdin = nil
    -- fail everything in flight instead of letting it time out
    for id, entry in pairs(M.pending) do
      if entry.cb then
        M.pending[id] = nil
        vim.schedule(function()
          entry.cb("agent connection lost (exit " .. tostring(out.code) .. ")", nil)
        end)
      else
        entry.msg = { error = { message = "agent connection lost" } }
        entry.done = true
      end
    end
    if dead_job then
      util.notify("agent connection lost (exit " .. tostring(out.code) .. ")", vim.log.levels.WARN)
    end
  end)
  stdin = job

  local hello = M.request("hello", {
    client_version = util.version(),
    proto = util.PROTO_VERSION,
  }, 30000)
  return hello
end

function M.shutdown()
  if job then
    local j = job
    job = nil
    pcall(function()
      j:write(nil) -- close stdin; the agent exits on EOF
    end)
  end
end

local function send(payload)
  if not job then
    error("[rnvim] not connected to an agent")
  end
  job:write(vim.json.encode(payload) .. "\n")
end

--- Fire-and-callback request; `cb(err, result)` runs on the main loop.
function M.request_async(method, params, cb)
  local id = M.next_id
  M.next_id = id + 1
  M.pending[id] = { cb = cb }
  local ok, err = pcall(send, { id = id, method = method, params = params or vim.empty_dict() })
  if not ok then
    M.pending[id] = nil
    vim.schedule(function()
      cb(tostring(err), nil)
    end)
  end
end

--- Send a request and wait for its response. Errors on timeout or remote error.
function M.request(method, params, timeout_ms)
  timeout_ms = timeout_ms or 30000
  local id = M.next_id
  M.next_id = id + 1
  M.pending[id] = { done = false }

  send({ id = id, method = method, params = params or vim.empty_dict() })

  local ok = vim.wait(timeout_ms, function()
    return M.pending[id].done
  end, 5)
  local entry = M.pending[id]
  M.pending[id] = nil

  if not ok then
    error(("[rnvim] %s timed out after %dms"):format(method, timeout_ms))
  end
  if entry.msg.error then
    error(("[rnvim] %s failed: %s"):format(method, entry.msg.error.message or "unknown error"))
  end
  return entry.msg.result
end

return M
