-- mason-registry as the install-recipe data source (a Lua port of the old
-- registry.rs).
--
-- The registry snapshot (mason-org/mason-registry's released
-- registry.json) is downloaded and cached locally; `plan_for` resolves a
-- package (by package name or binary name) into an install plan for a
-- concrete remote platform, and `install` orchestrates it end to end:
-- github artifacts are downloaded ON THE REMOTE by the agent's native
-- HTTP client (fetch.url), then unpacked by a network-free script;
-- npm/golang run entirely on the remote through its own package manager
-- and mirrors. Versions come pinned from the registry purl — never
-- "latest".

local util = require("rnvim.util")

local M = {}

local REGISTRY_URL = "https://github.com/mason-org/mason-registry/releases/latest/download/registry.json.zip"

local function registry_file()
  return util.home("registry", "registry.json")
end

--- Download + unpack the registry snapshot into the local cache.
function M.update()
  local dir = util.ensure_dir(util.home("registry"))
  local zip = vim.fs.joinpath(dir, "registry.json.zip")
  util.notify("fetching mason registry...")
  local dl = vim.system({ "curl", "-fsSL", "-o", zip, REGISTRY_URL }):wait()
  if dl.code ~= 0 then
    error("[rnvim] registry download failed: " .. (dl.stderr or ""))
  end
  local unzip = vim.system({ "unzip", "-p", zip, "registry.json" }):wait()
  if unzip.code ~= 0 or not unzip.stdout or unzip.stdout == "" then
    error("[rnvim] could not unpack registry.json from " .. zip)
  end
  local f = assert(io.open(registry_file(), "w"))
  f:write(unzip.stdout)
  f:close()
  os.remove(zip)
  util.notify("registry cached at " .. registry_file())
  return registry_file()
end

local function load()
  if not vim.uv.fs_stat(registry_file()) then
    M.update()
  end
  local f = assert(io.open(registry_file(), "r"))
  local text = f:read("*a")
  f:close()
  return vim.json.decode(text)
end

--- Find a package by its name, or by a key in its `bin` table (the LSP
--- layer asks for binaries like "pyright-langserver").
local function find(packages, name)
  for _, p in ipairs(packages) do
    if p.name == name then
      return p
    end
  end
  for _, p in ipairs(packages) do
    if type(p.bin) == "table" and p.bin[name] ~= nil then
      return p
    end
  end
  return nil
end

local function percent_decode(s)
  return (s:gsub("%%(%x%x)", function(hex)
    return string.char(tonumber(hex, 16))
  end))
end

--- Parse `pkg:<type>/<path>@<version>` (path percent-decoded).
function M.parse_purl(id)
  local rest = id:match("^pkg:(.*)$")
  if not rest then
    error("not a purl: " .. id)
  end
  rest = rest:match("^([^?]*)") or rest
  local kind, tail = rest:match("^([^/]+)/(.*)$")
  if not kind then
    error("purl missing type: " .. id)
  end
  local path, version = tail:match("^(.*)@([^@]*)$")
  if not path then
    error("purl missing version: " .. id)
  end
  return { kind = kind, path = percent_decode(path), version = percent_decode(version) }
end

--- mason target id → `uname -sm` outputs it corresponds to.
local function target_unames(target)
  local map = {
    darwin_arm64 = { "Darwin arm64" },
    darwin_x64 = { "Darwin x86_64" },
    linux_x64 = { "Linux x86_64" },
    linux_x64_gnu = { "Linux x86_64" },
    linux_arm64 = { "Linux aarch64" },
    linux_arm64_gnu = { "Linux aarch64" },
  }
  return map[target] or {}
end

--- How strongly to prefer an asset for a uname bucket (gnu > generic > musl).
local function target_priority(target)
  if vim.endswith(target, "_gnu") then
    return 3
  elseif vim.endswith(target, "_musl") then
    return 1
  end
  return 2
end

local function template(s, version)
  return (s:gsub("{{%s*version%s*}}", version))
end

--- The requested key if present in the package's `bin` table, else its
--- first key.
local function bin_key(pkg, preferred)
  local bins = pkg.bin
  if type(bins) ~= "table" then
    error("package has no bin table")
  end
  if bins[preferred] ~= nil then
    return preferred
  end
  local keys = vim.tbl_keys(bins)
  table.sort(keys)
  if #keys == 0 then
    error("package bin table is empty")
  end
  return keys[1]
end

--- Pick the best release asset (file, bin) for `uname_sm`, or nil.
local function pick_asset(pkg, uname_sm)
  local assets = pkg.source and pkg.source.asset
  if not assets then
    error("github source without assets")
  end
  if assets.file or assets.target then -- single-asset form
    assets = { assets }
  end
  local best
  for _, asset in ipairs(assets) do
    local file = asset.file
    if type(file) == "string" then -- multi-file assets unsupported
      local targets = type(asset.target) == "table" and asset.target or { asset.target }
      for _, t in ipairs(targets) do
        if type(t) == "string" and vim.list_contains(target_unames(t), uname_sm) then
          local prio = target_priority(t)
          if not best or prio > best.prio then
            best = { prio = prio, file = file, bin = asset.bin }
          end
        end
      end
    end
  end
  return best
end

local function npm_script(pkg, purl, requested)
  local key = bin_key(pkg, requested)
  return ([[set -e
command -v npm >/dev/null 2>&1 || { echo "%s needs node/npm on this host" >&2; exit 1; }
mkdir -p "$RNVIM_TOOLS/npm"
npm install --silent --prefix "$RNVIM_TOOLS/npm" "%s@%s" >/dev/null
[ -x "$RNVIM_TOOLS/npm/node_modules/.bin/%s" ] || { echo "npm install finished but %s missing" >&2; exit 1; }
echo "$RNVIM_TOOLS/npm/node_modules/.bin/%s"
]]):format(purl.path, purl.path, purl.version, key, key, key)
end

local function golang_script(pkg, purl, requested)
  local key = bin_key(pkg, requested)
  return ([[set -e
command -v go >/dev/null 2>&1 || { echo "%s needs a go toolchain on this host" >&2; exit 1; }
GOBIN="$RNVIM_TOOLS_BIN" go install "%s@%s"
echo "$RNVIM_TOOLS_BIN/%s"
]]):format(key, purl.path, purl.version, key)
end

--- Pick the release asset matching `uname_sm` and emit the network-free
--- unpack script that expects the file staged at ~/.rnvim/stage/.
local function staged_github_plan(pkg, purl, requested, uname_sm)
  local asset = pick_asset(pkg, uname_sm)
  if not asset then
    error(("no release asset for platform %q"):format(uname_sm))
  end
  local name = pkg.name or requested
  local key = bin_key(pkg, requested)

  -- mason file syntax: "remote", "remote:localname" (rename), or
  -- "remote:subdir/" (extract INTO that directory inside the package).
  local remote_file, stage_name, extract_sub
  local r, sub = asset.file:match("^(.-):(.*)$")
  if r and vim.endswith(sub, "/") then
    remote_file = template(r, purl.version)
    stage_name = remote_file:match("([^/]+)$") or remote_file
    extract_sub = sub:gsub("/+$", "")
  elseif r and sub ~= "" then
    remote_file = template(r, purl.version)
    stage_name = template(sub, purl.version)
    extract_sub = ""
  else
    remote_file = template(asset.file, purl.version)
    stage_name = remote_file
    extract_sub = ""
  end
  local bin_rel = stage_name
  if type(asset.bin) == "string" then
    bin_rel = template((asset.bin:gsub("^exec:", "")), purl.version)
  end
  local url = ("https://github.com/%s/releases/download/%s/%s"):format(purl.path, purl.version, remote_file)
  local extract_dir = extract_sub == "" and "$pkg" or ("$pkg/" .. extract_sub)
  local stage_stem = stage_name:gsub("%.gz$", "")

  local script = ([[set -e
staged="$HOME/.rnvim/stage/%s"
[ -f "$staged" ] || { echo "staged file missing: $staged" >&2; exit 1; }
pkg="$RNVIM_TOOLS/%s"
rm -rf "$pkg" && mkdir -p "%s"
bin_rel="%s"
case "%s" in
  *.tar.gz|*.tgz) tar xzf "$staged" -C "%s" ;;
  *.tar.xz)       tar xJf "$staged" -C "%s" ;;
  *.tar)          tar xf "$staged" -C "%s" ;;
  *.zip)
    command -v unzip >/dev/null 2>&1 || { echo "unzip required on this host" >&2; exit 1; }
    unzip -oq "$staged" -d "%s" ;;
  *.gz)
    bin_rel="%s"
    gunzip -c "$staged" > "$pkg/$bin_rel" ;;
  *)
    bin_rel="%s"
    cp "$staged" "$pkg/$bin_rel" ;;
esac
rm -f "$staged"
chmod +x "$pkg/$bin_rel" 2>/dev/null || true
[ -x "$pkg/$bin_rel" ] || { echo "unpacked but $bin_rel not found/executable" >&2; exit 1; }
printf '#!/bin/sh\nexec "%%s" "$@"\n' "$pkg/$bin_rel" > "$RNVIM_TOOLS_BIN/%s"
chmod +x "$RNVIM_TOOLS_BIN/%s"
echo "$RNVIM_TOOLS_BIN/%s"
]]):format(
    stage_name,
    name,
    extract_dir,
    bin_rel,
    stage_name,
    extract_dir,
    extract_dir,
    extract_dir,
    extract_dir,
    stage_stem,
    stage_name,
    key,
    key,
    key
  )

  return { kind = "staged", url = url, file = stage_name, script = script }
end

--- Resolve `name` into an install plan for a concrete remote platform
--- (`uname -sm` output): { kind = "staged", url, file, script } or
--- { kind = "remote", script }.
function M.plan_for(name, uname_sm, packages)
  packages = packages or load()
  local pkg = find(packages, name)
  if not pkg then
    error(("%s: not in the mason registry (try :RnvimRegistryUpdate)"):format(name))
  end
  local id = pkg.source and pkg.source.id
  if type(id) ~= "string" then
    error("package has no source id")
  end
  local purl = M.parse_purl(id)
  if purl.kind == "github" then
    return staged_github_plan(pkg, purl, name, uname_sm)
  elseif purl.kind == "npm" then
    return { kind = "remote", script = npm_script(pkg, purl, name) }
  elseif purl.kind == "golang" then
    return { kind = "remote", script = golang_script(pkg, purl, name) }
  end
  error(("%s uses unsupported source type %q — define it in vim.g.rnvim_lsp_recipes"):format(name, purl.kind))
end

--- Install `name` on the workspace host through the agent:
--- uname detect → plan → remote native download (fetch.url) → unpack
--- script via exec.run. `finish(err, path)` runs on the main loop.
function M.install(name, finish)
  local rpc = require("rnvim.rpc")
  rpc.request_async("exec.run", { script = "uname -sm" }, function(err, res)
    if err then
      return finish(err)
    end
    local uname_sm = vim.trim(res.stdout or "")
    if uname_sm == "" then
      return finish("could not detect remote platform")
    end
    local ok, plan = pcall(M.plan_for, name, uname_sm)
    if not ok then
      return finish(tostring(plan))
    end

    local function run_script(script)
      rpc.request_async("exec.run", { script = script }, function(rerr, rres)
        if rerr then
          return finish(rerr)
        end
        if rres.code ~= 0 then
          return finish((rres.stderr or ""):match("([^\n]+)%s*$") or ("exit " .. rres.code))
        end
        finish(nil, (rres.stdout or ""):match("([^\n]+)%s*\n?$"))
      end)
    end

    if plan.kind == "remote" then
      return run_script(plan.script)
    end
    rpc.request_async("fetch.url", { url = plan.url, path = "~/.rnvim/stage/" .. plan.file }, function(ferr)
      if ferr then
        return finish(("remote download of %s: %s"):format(plan.url, ferr))
      end
      run_script(plan.script)
    end)
  end)
end

-- exposed for tests
M._find = find

return M
