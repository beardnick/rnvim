-- Unit tests for the pure-logic modules. Run with:
--   nvim --headless --clean --cmd "set rtp+=." -l tests/unit_spec.lua

local failed = 0

local function check(name, fn)
  local ok, err = pcall(fn)
  if ok then
    print("ok      " .. name)
  else
    failed = failed + 1
    print("FAILED  " .. name .. ": " .. tostring(err))
  end
end

local function eq(got, want, what)
  if not vim.deep_equal(got, want) then
    error(("%s: got %s, want %s"):format(what or "value", vim.inspect(got), vim.inspect(want)), 2)
  end
end

-- ---------------------------------------------------------------- rewrite
local rw = require("rnvim.lsp_rewrite")
local WS = "/Users/me/.rnvim/ws/dev"

check("to_remote rules", function()
  eq(rw.to_remote(WS, "file://" .. WS .. "/home/q/main.go"), "file:///home/q/main.go")
  eq(rw.to_remote(WS, WS .. "/home/q"), "/home/q")
  eq(rw.to_remote(WS, WS), "/")
  eq(rw.to_remote(WS, "file://" .. WS), "file:///")
  -- boundary: /devX must not match /dev
  eq(rw.to_remote(WS, WS .. "X/nope"), nil)
  eq(rw.to_remote(WS, "unrelated text"), nil)
end)

check("to_local rules", function()
  eq(rw.to_local(WS, nil, "file:///home/q/main.go"), "file://" .. WS .. "/home/q/main.go")
  eq(rw.to_local(WS, "/home/q/proj", "/home/q/proj/lib/x.go"), WS .. "/home/q/proj/lib/x.go")
  -- plain paths outside the workspace root are left alone
  eq(rw.to_local(WS, "/home/q/proj", "/usr/lib/foo"), nil)
  -- without a captured root, plain paths are never touched
  eq(rw.to_local(WS, nil, "/home/q/proj/lib/x.go"), nil)
end)

check("rewrite walks nested tables without mutating", function()
  local msg = {
    textDocument = { uri = "file://" .. WS .. "/p/a.go", text = "file:// mentions inside code stay" },
    related = { { location = { uri = "file://" .. WS .. "/p/b.go" } } },
  }
  local out = rw.rewrite(msg, function(s)
    return rw.to_remote(WS, s)
  end)
  eq(out.textDocument.uri, "file:///p/a.go")
  eq(out.related[1].location.uri, "file:///p/b.go")
  assert(out.textDocument.text:find("stay"), "non-path text untouched")
  eq(msg.textDocument.uri, "file://" .. WS .. "/p/a.go", "input not mutated")
end)

check("captures initialize root", function()
  eq(rw.capture_remote_root({ rootUri = "file:///home/q/proj" }), "/home/q/proj")
  eq(rw.capture_remote_root({ workspaceFolders = { { uri = "file:///w1" } } }), "/w1")
  eq(rw.capture_remote_root({}), nil)
end)

-- --------------------------------------------------------------- registry
local registry = require("rnvim.registry")

check("parses purls", function()
  local p = registry.parse_purl("pkg:github/rust-lang/rust-analyzer@2025-01-01")
  eq({ p.kind, p.path, p.version }, { "github", "rust-lang/rust-analyzer", "2025-01-01" })
  p = registry.parse_purl("pkg:golang/golang.org/x/tools/gopls@v0.19.1")
  eq(p.path, "golang.org/x/tools/gopls")
  p = registry.parse_purl("pkg:npm/%40vue/language-server@2.0.0")
  eq(p.path, "@vue/language-server")
end)

check("finds by name or bin key", function()
  local pkgs = { { name = "pyright", bin = { ["pyright-langserver"] = "x" } } }
  assert(registry._find(pkgs, "pyright"))
  assert(registry._find(pkgs, "pyright-langserver"))
  eq(registry._find(pkgs, "nope"), nil)
end)

check("staged plan handles mason syntax", function()
  -- ":subdir/" extracts into a subdirectory; "exec:" prefixes strip;
  -- launcher is a wrapper shim, never a symlink (argv0-relative tools)
  local pkgs = {
    {
      name = "lls",
      bin = { lls = "{{source.asset.bin}}" },
      source = {
        id = "pkg:github/acme/lls@3.0.0",
        asset = {
          {
            target = "darwin_x64",
            file = "lls-3.0.0-darwin-x64.tar.gz:libexec/",
            bin = "exec:libexec/bin/lls",
          },
        },
      },
    },
  }
  local plan = registry.plan_for("lls", "Darwin x86_64", pkgs)
  eq(plan.kind, "staged")
  assert(vim.endswith(plan.url, "/download/3.0.0/lls-3.0.0-darwin-x64.tar.gz"), plan.url)
  eq(plan.file, "lls-3.0.0-darwin-x64.tar.gz", "staged under the archive basename")
  assert(plan.script:find("$pkg/libexec", 1, true), "extracts into the subdir")
  assert(plan.script:find('bin_rel="libexec/bin/lls"', 1, true), "exec: stripped")
  assert(plan.script:find('exec "%s"', 1, true), "wrapper shim, not symlink")
  assert(not plan.script:find("ln -sf", 1, true), "no symlinks")
end)

check("staged plan prefers gnu and templates version", function()
  local pkgs = {
    {
      name = "fake-ls",
      bin = { ["fake-ls"] = "{{source.asset.bin}}" },
      source = {
        id = "pkg:github/acme/fake-ls@v1.2.3",
        asset = {
          { target = { "linux_x64_gnu" }, file = "fake-linux.tar.gz", bin = "dist/fake-ls" },
          { target = "linux_x64_musl", file = "fake-musl.tar.gz", bin = "dist/fake-ls" },
          { target = "darwin_arm64", file = "fake-{{version}}.gz" },
        },
      },
    },
  }
  local plan = registry.plan_for("fake-ls", "Linux x86_64", pkgs)
  assert(plan.url:find("fake-linux.tar.gz", 1, true), "gnu asset chosen")
  assert(not plan.url:find("musl", 1, true), "musl not preferred over gnu")

  local mac = registry.plan_for("fake-ls", "Darwin arm64", pkgs)
  assert(mac.url:find("fake-v1.2.3.gz", 1, true), "version templated: " .. mac.url)
  assert(mac.script:find('bin_rel="fake-v1.2.3"', 1, true), ".gz stem is the binary")
end)

check("npm and golang plans", function()
  local pkgs = {
    {
      name = "pyright",
      bin = { ["pyright-langserver"] = "npm:x" },
      source = { id = "pkg:npm/pyright@1.1.0" },
    },
    {
      name = "gopls",
      bin = { gopls = "golang:gopls" },
      source = { id = "pkg:golang/golang.org/x/tools/gopls@v0.19.1" },
    },
  }
  local npm = registry.plan_for("pyright-langserver", "Linux x86_64", pkgs)
  eq(npm.kind, "remote")
  assert(npm.script:find("pyright@1.1.0", 1, true))
  assert(npm.script:find(".bin/pyright-langserver", 1, true))

  local go = registry.plan_for("gopls", "Linux x86_64", pkgs)
  assert(go.script:find('go install "golang.org/x/tools/gopls@v0.19.1"', 1, true))
end)

-- ---------------------------------------------------------------- remotes
check("parses ssh config with includes, skips wildcards", function()
  local dir = vim.fn.tempname()
  vim.fn.mkdir(dir .. "/.ssh", "p")
  local f = assert(io.open(dir .. "/.ssh/config", "w"))
  f:write("# comment\nHost dev-box staging\n  HostName 10.0.0.1\nHost *\n  User me\nInclude extra_config\n")
  f:close()
  f = assert(io.open(dir .. "/.ssh/extra_config", "w"))
  f:write("Host gpu-server\n")
  f:close()

  local out = {}
  require("rnvim.remotes")._collect_hosts(dir .. "/.ssh/config", dir, out, 0)
  eq(out, { "dev-box", "staging", "gpu-server" })
end)

-- ------------------------------------------------------------------- util
check("parses targets", function()
  local util = require("rnvim.util")
  eq(util.parse_target("dev-box:~/proj"), { host = "dev-box", path = "~/proj", is_local = false })
  eq(util.parse_target("user@10.0.0.1"), { host = "user@10.0.0.1", path = "", is_local = false })
  eq(util.parse_target("local:/tmp/x").is_local, true)
  eq(util.host_slug("user@host:22"), "user@host_22")
end)

if failed > 0 then
  print(("%d test(s) FAILED"):format(failed))
  os.exit(1)
end
print("all unit tests passed")
