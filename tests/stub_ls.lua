-- A stub language server for tests, run with:  nvim -l tests/stub_ls.lua <logfile>
--
-- Speaks just enough LSP over stdio to complete the handshake, advertises
-- full-text sync + save notifications (so clients send didOpen/didChange/
-- didSave), and appends one JSON line per received message to <logfile>:
--   { method, uri?, text? }
-- Tests then assert on the recorded traffic — the protocol-plane oracle.

local logfile = _G.arg and _G.arg[1] or error("usage: nvim -l stub_ls.lua <logfile>")
local log = assert(io.open(logfile, "w"))

local function read_frame()
  local len
  while true do
    local line = io.read("*l")
    if not line then
      return nil
    end
    line = line:gsub("\r$", "")
    if line == "" then
      break
    end
    local v = line:match("^[Cc]ontent%-[Ll]ength:%s*(%d+)")
    if v then
      len = tonumber(v)
    end
  end
  if not len then
    return nil
  end
  return io.read(len)
end

local function send(obj)
  local body = vim.json.encode(obj)
  io.write(("Content-Length: %d\r\n\r\n%s"):format(#body, body))
  io.flush()
end

local function record(msg)
  local p = msg.params or {}
  local entry = { method = msg.method }
  local doc = p.textDocument
  if type(doc) == "table" then
    entry.uri = doc.uri
    entry.text = doc.text -- didOpen carries full text here
  end
  if type(p.contentChanges) == "table" and p.contentChanges[1] then
    entry.text = p.contentChanges[1].text -- full-sync didChange
  end
  if type(p.text) == "string" then
    entry.text = p.text -- didSave with includeText
  end
  log:write(vim.json.encode(entry) .. "\n")
  log:flush()
end

while true do
  local frame = read_frame()
  if not frame then
    break
  end
  local ok, msg = pcall(vim.json.decode, frame)
  if ok and type(msg) == "table" then
    if msg.method then
      record(msg)
    end
    if msg.method == "initialize" then
      send({
        jsonrpc = "2.0",
        id = msg.id,
        result = {
          capabilities = {
            textDocumentSync = {
              openClose = true,
              change = 1, -- full-text didChange, so traffic captures content
              save = { includeText = true },
            },
          },
        },
      })
    elseif msg.method == "exit" then
      break
    elseif msg.id then
      send({ jsonrpc = "2.0", id = msg.id, result = vim.NIL })
    end
  end
end
log:close()
