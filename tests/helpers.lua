-- Shared test harness: assertions and a step-scenario runner.
-- Loaded by spec files via dofile (tests/ is not on the lua module path).

local M = {}

function M.eq(got, want, what)
  if not vim.deep_equal(got, want) then
    error(("%s: got %s, want %s"):format(what or "value", vim.inspect(got), vim.inspect(want)), 2)
  end
end

function M.contains(haystack, needle, what)
  if not (type(haystack) == "string" and haystack:find(needle, 1, true)) then
    error(("%s: %q not found in %s"):format(what or "contains", needle, vim.inspect(haystack)), 2)
  end
end

function M.truthy(v, what)
  if not v then
    error(("%s: expected truthy, got %s"):format(what or "value", vim.inspect(v)), 2)
  end
end

--- Poll `pred` until it returns a truthy value or `ms` elapses; returns
--- the value (vim.wait keeps the uv loop pumping, so async callbacks and
--- child-process IO make progress while we block).
function M.eventually(ms, pred, what)
  local result
  local ok = vim.wait(ms, function()
    result = pred()
    return result and true or false
  end, 50)
  if not ok then
    error(("%s: condition not met within %dms"):format(what or "eventually", ms), 2)
  end
  return result
end

--- Run named steps in order. Steps share state via the `ctx` table each
--- step receives. Aborts at the first failure (later steps depend on
--- earlier state) but always prints a per-step report first.
function M.scenario(name, steps)
  local ctx = {}
  print(("=== %s (%d steps)"):format(name, #steps))
  for i, step in ipairs(steps) do
    local ok, err = pcall(step[2], ctx)
    if ok then
      print(("ok   %d/%d %s"):format(i, #steps, step[1]))
    else
      print(("FAIL %d/%d %s\n     %s"):format(i, #steps, step[1], tostring(err)))
      print(("=== %s FAILED"):format(name))
      os.exit(1)
    end
  end
  print(("=== %s passed"):format(name))
end

return M
