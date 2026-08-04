-- End-to-end over the `local:` loopback: boot a remote-personality
-- instance, edit a file through the virtual fs, verify it lands on disk,
-- and exercise the remote finder. Run with an isolated HOME and a built
-- agent:
--   HOME=<tmp> RNVIM_AGENT_BIN=target/debug/rnvim-agent \
--     nvim --headless --clean --cmd "set rtp+=." -l tests/e2e.lua

local function fail(msg)
  print("FAILED  " .. msg)
  os.exit(1)
end

assert(vim.env.RNVIM_AGENT_BIN, "RNVIM_AGENT_BIN must point at a built agent")

-- a scratch "remote" directory with one pre-existing file
local workdir = vim.fn.tempname()
vim.fn.mkdir(workdir .. "/sub", "p")
local f = assert(io.open(workdir .. "/hello.txt", "w"))
f:write("hello from disk\n")
f:close()

-- boot the remote personality directly (what plugin/rnvim.lua does on VimEnter)
vim.g.rnvim = { target = "local:" .. workdir }
require("rnvim").setup()

local ws = require("rnvim.workspace").current()
if not ws then
  fail("workspace did not mount")
end

-- 1. reading an existing remote file through the virtual fs
vim.cmd.edit(ws.ws_root .. workdir .. "/hello.txt")
local lines = vim.api.nvim_buf_get_lines(0, 0, -1, false)
if lines[1] ~= "hello from disk" then
  fail("read through virtual fs: got " .. vim.inspect(lines))
end

-- 2. creating + writing a new remote file
vim.cmd.edit(ws.ws_root .. workdir .. "/sub/note.txt")
vim.api.nvim_buf_set_lines(0, 0, -1, false, { "written through rnvim" })
vim.cmd.write()
local g = io.open(workdir .. "/sub/note.txt", "r")
if not g then
  fail("note.txt was not created on disk")
end
local content = g:read("*a")
g:close()
if not content:find("written through rnvim", 1, true) then
  fail("note.txt content wrong: " .. content)
end

-- 3. remote finder sees both files
local res = require("rnvim.rpc").request("find.files", { root = workdir, query = "note", limit = 10 })
local found = false
for _, file in ipairs(res.files or {}) do
  if file:find("note.txt", 1, true) then
    found = true
  end
end
if not found then
  fail("find.files did not return note.txt: " .. vim.inspect(res))
end

-- 4. the session registered itself
local sessions = require("rnvim.sessions").list()
if #sessions ~= 1 then
  fail("expected 1 registered session, got " .. #sessions)
end
if not sessions[1].target:find(workdir, 1, true) then
  fail("session target wrong: " .. vim.inspect(sessions[1]))
end

-- 5. directory listing buffer
vim.cmd.edit(ws.ws_root .. workdir)
local dirlines = vim.api.nvim_buf_get_lines(0, 0, -1, false)
local has_sub = false
for _, l in ipairs(dirlines) do
  if l == "sub/" then
    has_sub = true
  end
end
if not has_sub then
  fail("directory buffer missing sub/: " .. vim.inspect(dirlines))
end

print("e2e passed")
