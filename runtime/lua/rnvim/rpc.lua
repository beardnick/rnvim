-- JSON-lines RPC client over the session unix socket.
-- Requests are synchronous from the caller's point of view: we block with
-- vim.wait() (which keeps pumping the uv loop) until the response arrives.

local M = {
  pending = {},
  next_id = 1,
}

local pipe
local rxbuf = ""

local function on_data(data)
  rxbuf = rxbuf .. data
  while true do
    local nl = rxbuf:find("\n", 1, true)
    if not nl then
      break
    end
    local line = rxbuf:sub(1, nl - 1)
    rxbuf = rxbuf:sub(nl + 1)
    if line ~= "" then
      local ok, msg = pcall(vim.json.decode, line)
      if ok and type(msg) == "table" and msg.id and M.pending[msg.id] then
        local entry = M.pending[msg.id]
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
    end
  end
end

function M.connect(path)
  pipe = vim.uv.new_pipe(false)
  local done, conn_err = false, nil
  pipe:connect(path, function(err)
    conn_err = err
    done = true
  end)
  vim.wait(5000, function()
    return done
  end, 10)
  if not done or conn_err then
    error("[rnvim] cannot connect to session socket " .. path .. ": " .. tostring(conn_err))
  end
  pipe:read_start(function(err, data)
    if err or not data then
      return
    end
    on_data(data)
  end)
end

--- Fire-and-callback request; `cb(err, result)` runs on the main loop.
--- Never blocks — this is what interactive UIs (picker) must use.
function M.request_async(method, params, cb)
  local id = M.next_id
  M.next_id = id + 1
  M.pending[id] = { cb = cb }
  pipe:write(vim.json.encode({ id = id, method = method, params = params or vim.empty_dict() }) .. "\n")
end

--- Send a request and wait for its response. Errors on timeout or remote error.
function M.request(method, params, timeout_ms)
  timeout_ms = timeout_ms or 30000
  local id = M.next_id
  M.next_id = id + 1
  M.pending[id] = { done = false }

  pipe:write(vim.json.encode({ id = id, method = method, params = params or vim.empty_dict() }) .. "\n")

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
