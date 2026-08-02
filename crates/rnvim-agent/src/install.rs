//! Remote LSP-server auto-install: recipes for servers with self-contained
//! release artifacts (or a resident toolchain), installed under
//! ~/.rnvim/tools/. Runs through the user's login shell so go/npm from
//! profile PATHs are found. The caller runs each install on its own thread:
//! downloads must never block the agent's request loop.

use anyhow::{anyhow, bail, Context, Result};

/// PATH fragment where installed tools land; exec.which and the LSP proxy
/// both prepend this.
pub const TOOLS_PATH: &str = "$HOME/.rnvim/tools/bin:$HOME/.rnvim/tools/npm/node_modules/.bin";

/// A recipe prints the installed binary's absolute path as its last line.
fn recipe(name: &str) -> Option<&'static str> {
    Some(match name {
        "rust-analyzer" => {
            r#"set -e
mkdir -p "$HOME/.rnvim/tools/bin"
case "$(uname -sm)" in
  "Linux x86_64")  t=x86_64-unknown-linux-gnu ;;
  "Linux aarch64") t=aarch64-unknown-linux-gnu ;;
  "Darwin arm64")  t=aarch64-apple-darwin ;;
  "Darwin x86_64") t=x86_64-apple-darwin ;;
  *) echo "unsupported platform: $(uname -sm)" >&2; exit 1 ;;
esac
curl -fsSL "https://github.com/rust-lang/rust-analyzer/releases/latest/download/rust-analyzer-$t.gz" \
  | gunzip > "$HOME/.rnvim/tools/bin/rust-analyzer"
chmod +x "$HOME/.rnvim/tools/bin/rust-analyzer"
echo "$HOME/.rnvim/tools/bin/rust-analyzer""#
        }
        "lua-language-server" => {
            r#"set -e
mkdir -p "$HOME/.rnvim/tools/bin" "$HOME/.rnvim/tools/lua-language-server"
case "$(uname -sm)" in
  "Linux x86_64")  a=linux-x64 ;;
  "Linux aarch64") a=linux-arm64 ;;
  "Darwin arm64")  a=darwin-arm64 ;;
  "Darwin x86_64") a=darwin-x64 ;;
  *) echo "unsupported platform: $(uname -sm)" >&2; exit 1 ;;
esac
url=$(curl -fsSL https://api.github.com/repos/LuaLS/lua-language-server/releases/latest \
  | grep -o "\"browser_download_url\": *\"[^\"]*$a\.tar\.gz\"" | grep -o "https[^\"]*" | head -1)
[ -n "$url" ] || { echo "no release asset for $a" >&2; exit 1; }
curl -fsSL "$url" | tar xz -C "$HOME/.rnvim/tools/lua-language-server"
ln -sf "$HOME/.rnvim/tools/lua-language-server/bin/lua-language-server" "$HOME/.rnvim/tools/bin/lua-language-server"
echo "$HOME/.rnvim/tools/bin/lua-language-server""#
        }
        "gopls" => {
            r#"set -e
command -v go >/dev/null 2>&1 || { echo "gopls needs a go toolchain on this host" >&2; exit 1; }
mkdir -p "$HOME/.rnvim/tools/bin"
GOBIN="$HOME/.rnvim/tools/bin" go install golang.org/x/tools/gopls@latest
echo "$HOME/.rnvim/tools/bin/gopls""#
        }
        "clangd" => {
            r#"set -e
command -v unzip >/dev/null 2>&1 || { echo "clangd install needs unzip on this host" >&2; exit 1; }
mkdir -p "$HOME/.rnvim/tools/bin" "$HOME/.rnvim/tools/clangd"
case "$(uname -s)" in
  Linux)  a=linux ;;
  Darwin) a=mac ;;
  *) echo "unsupported platform" >&2; exit 1 ;;
esac
url=$(curl -fsSL https://api.github.com/repos/clangd/clangd/releases/latest \
  | grep -o "\"browser_download_url\": *\"[^\"]*clangd-$a-[0-9.]*\.zip\"" | grep -o "https[^\"]*" | head -1)
[ -n "$url" ] || { echo "no clangd release asset for $a" >&2; exit 1; }
tmp=$(mktemp) && curl -fsSL "$url" -o "$tmp"
unzip -oq "$tmp" -d "$HOME/.rnvim/tools/clangd" && rm -f "$tmp"
bin=$(ls -d "$HOME"/.rnvim/tools/clangd/clangd_*/bin/clangd | head -1)
ln -sf "$bin" "$HOME/.rnvim/tools/bin/clangd"
echo "$HOME/.rnvim/tools/bin/clangd""#
        }
        "pyright" => {
            r#"set -e
command -v npm >/dev/null 2>&1 || { echo "pyright needs node/npm on this host" >&2; exit 1; }
mkdir -p "$HOME/.rnvim/tools/npm"
npm install --silent --prefix "$HOME/.rnvim/tools/npm" pyright >/dev/null
echo "$HOME/.rnvim/tools/npm/node_modules/.bin/pyright-langserver""#
        }
        "typescript-language-server" => {
            r#"set -e
command -v npm >/dev/null 2>&1 || { echo "ts_ls needs node/npm on this host" >&2; exit 1; }
mkdir -p "$HOME/.rnvim/tools/npm"
npm install --silent --prefix "$HOME/.rnvim/tools/npm" typescript-language-server typescript >/dev/null
echo "$HOME/.rnvim/tools/npm/node_modules/.bin/typescript-language-server""#
        }
        _ => return None,
    })
}

/// Run an install recipe to completion; returns the installed binary path.
pub fn install(name: &str) -> Result<String> {
    let script = recipe(name)
        .ok_or_else(|| anyhow!("no install recipe for {name:?} (install it manually)"))?;
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    let out = std::process::Command::new(shell)
        .arg("-lc")
        .arg(script)
        .output()
        .context("run install recipe")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!(
            "install {name} failed: {}",
            err.lines().last().unwrap_or("unknown error").trim()
        );
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let path = stdout
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .context("recipe produced no path")?;
    Ok(path.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_recipes_exist_and_print_a_path() {
        for name in [
            "rust-analyzer",
            "lua-language-server",
            "gopls",
            "clangd",
            "pyright",
            "typescript-language-server",
        ] {
            let r = recipe(name).expect(name);
            assert!(
                r.contains("echo \"$HOME/.rnvim/tools/"),
                "{name} must print its path"
            );
        }
    }

    #[test]
    fn unknown_recipe_is_a_clean_error() {
        let err = install("definitely-not-a-server").unwrap_err().to_string();
        assert!(err.contains("no install recipe"), "{err}");
    }
}
