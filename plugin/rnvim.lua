-- Plugin entry: boots on startup. Remote instances (vim.g.rnvim set via
-- --cmd before config load) connect their workspace; local instances just
-- get :RnvimConnect.

if vim.g.loaded_rnvim then
  return
end
vim.g.loaded_rnvim = true

-- Boot after the user's config has fully loaded (so LSP capability
-- inheritance sees their completion engine and their vim.lsp.config
-- registrations exist).
vim.api.nvim_create_autocmd("VimEnter", {
  group = vim.api.nvim_create_augroup("RnvimBoot", { clear = true }),
  once = true,
  callback = function()
    require("rnvim").setup()
  end,
})
