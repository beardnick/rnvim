-- User-defined install recipes: full-control escape hatch. A recipe here
-- is a POSIX script sent verbatim to the remote agent's exec.run (so it
-- runs entirely on the remote — including any network it chooses to use):
--
--   vim.g.rnvim_lsp_recipes = { ["my-ls"] = [[...script...]] }
--
-- Without a user recipe, installs go through the broker's session.install:
-- the mason-registry plan is resolved locally, artifacts are downloaded on
-- the LOCAL machine (cached, GitHub-reachable side) and staged to the
-- remote through the agent — the remote needs no GitHub access. npm/golang
-- packages run remotely via the remote's own package manager and mirrors.

local M = {}

function M.user_script(bin)
  local user = vim.g.rnvim_lsp_recipes
  if type(user) == "table" and type(user[bin]) == "string" then
    return user[bin]
  end
  return nil
end

return M
