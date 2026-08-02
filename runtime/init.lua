-- rnvim managed runtime entrypoint. Loaded via `nvim -u` by the rnvim client.

local runtime = vim.env.RNVIM_RUNTIME
if runtime and runtime ~= "" then
  vim.opt.runtimepath:prepend(runtime)
end

vim.o.number = true
vim.o.termguicolors = true
vim.o.mouse = "a"
vim.o.ignorecase = true
vim.o.smartcase = true
vim.o.signcolumn = "yes"
vim.o.updatetime = 300

require("rnvim").setup()

-- User overlay: keymaps, colorscheme, pure-buffer plugins.
-- Lives outside the managed runtime so upgrades never touch it.
local user_init = vim.fn.stdpath("config") .. "/user/init.lua"
if vim.uv.fs_stat(user_init) then
  local ok, err = pcall(dofile, user_init)
  if not ok then
    vim.notify("[rnvim] user config error: " .. tostring(err), vim.log.levels.ERROR)
  end
end
