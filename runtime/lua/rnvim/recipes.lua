-- Install-recipe resolution: what to run on the remote host to install an
-- LSP server. Recipes are always resolved on the editor side and sent to
-- the agent's generic exec.run — the agent has no server knowledge.
--
-- Resolution order:
--   1. vim.g.rnvim_lsp_recipes[bin]  — user-defined script (full control)
--   2. mason-registry               — `rnvim registry script <bin>` resolves
--      the package (versions pinned by the registry snapshot) and emits a
--      self-contained POSIX script
--
-- Convention: a recipe installs under $RNVIM_TOOLS (provided by the agent,
-- on PATH for exec.which and the LSP proxy) and prints the installed
-- binary's path as its last stdout line.

local M = {}

--- Resolve asynchronously: cb(script | nil, err | nil). The first registry
--- lookup downloads the snapshot (~10MB), so this must never block the UI.
function M.get(bin, cb)
  local user = vim.g.rnvim_lsp_recipes
  if type(user) == "table" and type(user[bin]) == "string" then
    cb(user[bin], nil)
    return
  end

  local rnvim_bin = vim.env.RNVIM_BIN
  if not rnvim_bin or rnvim_bin == "" then
    cb(nil, "RNVIM_BIN not set")
    return
  end
  vim.system(
    { rnvim_bin, "registry", "script", bin },
    { text = true },
    vim.schedule_wrap(function(res)
      if res.code == 0 and res.stdout ~= "" then
        cb(res.stdout, nil)
      else
        cb(nil, vim.trim(res.stderr or ""):match("([^\n]+)%s*$") or "registry lookup failed")
      end
    end)
  )
end

return M
