-- Shared e2e boot: build a scratch "remote" directory and start the
-- remote personality over the local: loopback, exactly as
-- plugin/rnvim.lua would on VimEnter. Returns { workdir, ws }.

return function()
  assert(vim.env.RNVIM_AGENT_BIN, "RNVIM_AGENT_BIN must point at a built agent")

  local workdir = vim.fn.tempname()
  vim.fn.mkdir(workdir .. "/sub", "p")
  local f = assert(io.open(workdir .. "/hello.txt", "w"))
  f:write("hello from disk\n")
  f:close()

  vim.g.rnvim = { target = "local:" .. workdir }
  require("rnvim").setup()

  local ws = require("rnvim.workspace").current()
  assert(ws, "workspace did not mount")
  return { workdir = workdir, ws = ws }
end
