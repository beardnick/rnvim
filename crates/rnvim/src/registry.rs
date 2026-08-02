//! mason-registry as the install-recipe data source.
//!
//! The registry snapshot (mason-org/mason-registry's released
//! registry.json) is downloaded and cached locally; `rnvim registry script
//! <name>` resolves a package (by package name or binary name) and emits a
//! self-contained POSIX install script for the agent's generic exec.run.
//! Versions come pinned from the registry purl — no "latest" downloads.
//!
//! Supported source types: pkg:github (release assets), pkg:npm,
//! pkg:golang. Anything else gets a clear "define your own recipe" error.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;

const REGISTRY_URL: &str =
    "https://github.com/mason-org/mason-registry/releases/latest/download/registry.json.zip";

fn registry_dir() -> Result<PathBuf> {
    Ok(crate::nvim::rnvim_home()?.join("registry"))
}

fn registry_file() -> Result<PathBuf> {
    Ok(registry_dir()?.join("registry.json"))
}

/// Download + unpack the registry snapshot into the local cache.
pub fn update() -> Result<PathBuf> {
    let dir = registry_dir()?;
    std::fs::create_dir_all(&dir)?;
    let zip = dir.join("registry.json.zip");
    eprintln!("[rnvim] fetching mason registry...");
    let status = Command::new("curl")
        .args(["-fsSL", "-o"])
        .arg(&zip)
        .arg(REGISTRY_URL)
        .status()
        .context("spawn curl")?;
    if !status.success() {
        bail!("registry download failed: {REGISTRY_URL}");
    }
    let out = Command::new("unzip")
        .arg("-p")
        .arg(&zip)
        .arg("registry.json")
        .output()
        .context("spawn unzip")?;
    if !out.status.success() || out.stdout.is_empty() {
        bail!("could not unpack registry.json from {}", zip.display());
    }
    let file = registry_file()?;
    std::fs::write(&file, &out.stdout)?;
    let _ = std::fs::remove_file(&zip);
    eprintln!("[rnvim] registry cached at {}", file.display());
    Ok(file)
}

fn load() -> Result<Vec<Value>> {
    let file = registry_file()?;
    if !file.exists() {
        update()?;
    }
    let text = std::fs::read_to_string(registry_file()?)?;
    serde_json::from_str(&text).context("parse registry.json")
}

/// Find a package by its name, or by a key in its `bin` table (the LSP
/// layer asks for binaries like "pyright-langserver").
fn find<'a>(packages: &'a [Value], name: &str) -> Option<&'a Value> {
    packages
        .iter()
        .find(|p| p.get("name").and_then(Value::as_str) == Some(name))
        .or_else(|| {
            packages.iter().find(|p| {
                p.get("bin")
                    .and_then(Value::as_object)
                    .is_some_and(|b| b.contains_key(name))
            })
        })
}

struct Purl {
    kind: String,
    /// Everything between the type and the version (owner/repo, npm name,
    /// go module path), percent-decoded.
    path: String,
    version: String,
}

fn percent_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v as char);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn parse_purl(id: &str) -> Result<Purl> {
    let rest = id.strip_prefix("pkg:").context("not a purl")?;
    let rest = rest.split('?').next().unwrap_or(rest);
    let (kind, rest) = rest.split_once('/').context("purl missing type")?;
    let (path, version) = rest.rsplit_once('@').context("purl missing version")?;
    Ok(Purl {
        kind: kind.to_string(),
        path: percent_decode(path),
        version: percent_decode(version),
    })
}

/// mason target id → `uname -sm` output(s) it corresponds to.
fn target_unames(target: &str) -> &'static [&'static str] {
    match target {
        "darwin_arm64" => &["Darwin arm64"],
        "darwin_x64" => &["Darwin x86_64"],
        "linux_x64" | "linux_x64_gnu" => &["Linux x86_64"],
        "linux_arm64" | "linux_arm64_gnu" => &["Linux aarch64"],
        _ => &[],
    }
}

/// How strongly to prefer an asset for a uname bucket (gnu > generic > musl).
fn target_priority(target: &str) -> u8 {
    if target.ends_with("_gnu") {
        3
    } else if target.ends_with("_musl") {
        1
    } else {
        2
    }
}

fn template(s: &str, version: &str) -> String {
    s.replace("{{version}}", version)
        .replace("{{ version }}", version)
}

/// The first key of the package's `bin` table when `preferred` is absent.
fn bin_key(pkg: &Value, preferred: &str) -> Result<String> {
    let bins = pkg
        .get("bin")
        .and_then(Value::as_object)
        .context("package has no bin table")?;
    if bins.contains_key(preferred) {
        return Ok(preferred.to_string());
    }
    bins.keys()
        .next()
        .cloned()
        .context("package bin table is empty")
}

fn github_script(pkg: &Value, purl: &Purl, requested: &str) -> Result<String> {
    let assets = pkg
        .get("source")
        .and_then(|s| s.get("asset"))
        .context("github source without assets")?;
    let asset_list: Vec<&Value> = match assets {
        Value::Array(items) => items.iter().collect(),
        one => vec![one],
    };

    // uname bucket → (priority, file, bin-in-package)
    let mut chosen: std::collections::HashMap<&str, (u8, String, Option<String>)> =
        std::collections::HashMap::new();
    for asset in &asset_list {
        let Some(file) = asset.get("file").and_then(Value::as_str) else {
            continue; // multi-file assets unsupported
        };
        let bin = asset.get("bin").and_then(Value::as_str).map(String::from);
        let targets: Vec<&str> = match asset.get("target") {
            Some(Value::String(t)) => vec![t.as_str()],
            Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).collect(),
            _ => continue,
        };
        for t in targets {
            let prio = target_priority(t);
            for uname in target_unames(t) {
                let entry = chosen.entry(uname).or_insert((0, String::new(), None));
                if prio > entry.0 {
                    *entry = (prio, file.to_string(), bin.clone());
                }
            }
        }
    }
    if chosen.is_empty() {
        bail!("no usable release asset for any supported platform");
    }

    let name = pkg.get("name").and_then(Value::as_str).unwrap_or(requested);
    let key = bin_key(pkg, requested)?;
    let mut cases = String::new();
    for (uname, (_, file, bin)) in &chosen {
        // "remote.ext:localname" renames on download
        let (remote, local) = match file.split_once(':') {
            Some((r, l)) if !l.is_empty() => (template(r, &purl.version), l.to_string()),
            _ => (template(file, &purl.version), template(file, &purl.version)),
        };
        let bin_rel = bin
            .as_deref()
            .map(|b| template(b, &purl.version))
            .unwrap_or_else(|| local.clone());
        cases.push_str(&format!(
            "  \"{uname}\") file=\"{remote}\"; local=\"{local}\"; bin_rel=\"{bin_rel}\" ;;\n"
        ));
    }

    Ok(format!(
        r#"set -e
case "$(uname -sm)" in
{cases}  *) echo "no {name} build for $(uname -sm)" >&2; exit 1 ;;
esac
pkg="$RNVIM_TOOLS/{name}"
rm -rf "$pkg" && mkdir -p "$pkg"
url="https://github.com/{repo}/releases/download/{version}/$file"
case "$local" in
  *.tar.gz|*.tgz) curl -fsSL "$url" | tar xz -C "$pkg" ;;
  *.tar.xz)       curl -fsSL "$url" | tar xJ -C "$pkg" ;;
  *.tar)          curl -fsSL "$url" | tar x -C "$pkg" ;;
  *.zip)
    command -v unzip >/dev/null 2>&1 || {{ echo "unzip required on this host" >&2; exit 1; }}
    tmp=$(mktemp) && curl -fsSL "$url" -o "$tmp" && unzip -oq "$tmp" -d "$pkg" && rm -f "$tmp" ;;
  *.gz)
    bin_rel="${{local%.gz}}"
    curl -fsSL "$url" | gunzip > "$pkg/$bin_rel" ;;
  *)
    bin_rel="$local"
    curl -fsSL "$url" -o "$pkg/$local" ;;
esac
chmod +x "$pkg/$bin_rel" 2>/dev/null || true
[ -x "$pkg/$bin_rel" ] || {{ echo "installed but $bin_rel not found/executable" >&2; exit 1; }}
ln -sf "$pkg/$bin_rel" "$RNVIM_TOOLS_BIN/{key}"
echo "$RNVIM_TOOLS_BIN/{key}"
"#,
        repo = purl.path,
        version = purl.version,
    ))
}

fn npm_script(pkg: &Value, purl: &Purl, requested: &str) -> Result<String> {
    let key = bin_key(pkg, requested)?;
    Ok(format!(
        r#"set -e
command -v npm >/dev/null 2>&1 || {{ echo "{name} needs node/npm on this host" >&2; exit 1; }}
mkdir -p "$RNVIM_TOOLS/npm"
npm install --silent --prefix "$RNVIM_TOOLS/npm" "{name}@{version}" >/dev/null
[ -x "$RNVIM_TOOLS/npm/node_modules/.bin/{key}" ] || {{ echo "npm install finished but {key} missing" >&2; exit 1; }}
echo "$RNVIM_TOOLS/npm/node_modules/.bin/{key}"
"#,
        name = purl.path,
        version = purl.version,
    ))
}

fn golang_script(pkg: &Value, purl: &Purl, requested: &str) -> Result<String> {
    let key = bin_key(pkg, requested)?;
    Ok(format!(
        r#"set -e
command -v go >/dev/null 2>&1 || {{ echo "{key} needs a go toolchain on this host" >&2; exit 1; }}
GOBIN="$RNVIM_TOOLS_BIN" go install "{module}@{version}"
echo "$RNVIM_TOOLS_BIN/{key}"
"#,
        module = purl.path,
        version = purl.version,
    ))
}

pub fn script_for(name: &str) -> Result<String> {
    let packages = load()?;
    let pkg = find(&packages, name)
        .ok_or_else(|| anyhow!("{name}: not in the mason registry (try: rnvim registry update)"))?;
    let id = pkg
        .get("source")
        .and_then(|s| s.get("id"))
        .and_then(Value::as_str)
        .context("package has no source id")?;
    let purl = parse_purl(id)?;
    match purl.kind.as_str() {
        "github" => github_script(pkg, &purl, name),
        "npm" => npm_script(pkg, &purl, name),
        "golang" => golang_script(pkg, &purl, name),
        other => bail!(
            "{name} uses unsupported source type {other:?} — define it in vim.g.rnvim_lsp_recipes"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fake_github_pkg() -> Value {
        json!({
            "name": "fake-ls",
            "bin": { "fake-ls": "{{source.asset.bin}}" },
            "source": {
                "id": "pkg:github/acme/fake-ls@v1.2.3",
                "asset": [
                    { "target": ["linux_x64_gnu"], "file": "fake-linux.tar.gz", "bin": "dist/fake-ls" },
                    { "target": "linux_x64_musl", "file": "fake-musl.tar.gz", "bin": "dist/fake-ls" },
                    { "target": "darwin_arm64", "file": "fake-{{version}}.gz" }
                ]
            }
        })
    }

    #[test]
    fn parses_purls() {
        let p = parse_purl("pkg:github/rust-lang/rust-analyzer@2025-01-01").unwrap();
        assert_eq!(
            (p.kind.as_str(), p.path.as_str(), p.version.as_str()),
            ("github", "rust-lang/rust-analyzer", "2025-01-01")
        );
        let p = parse_purl("pkg:golang/golang.org/x/tools/gopls@v0.19.1").unwrap();
        assert_eq!(p.path, "golang.org/x/tools/gopls");
        let p = parse_purl("pkg:npm/%40vue/language-server@2.0.0").unwrap();
        assert_eq!(p.path, "@vue/language-server");
    }

    #[test]
    fn github_script_prefers_gnu_and_templates_version() {
        let pkg = fake_github_pkg();
        let purl = parse_purl("pkg:github/acme/fake-ls@v1.2.3").unwrap();
        let s = github_script(&pkg, &purl, "fake-ls").unwrap();
        assert!(s.contains("fake-linux.tar.gz"), "gnu asset chosen: {s}");
        assert!(!s.contains("fake-musl"), "musl not preferred over gnu");
        assert!(s.contains("fake-v1.2.3.gz"), "version templated");
        assert!(s.contains("releases/download/v1.2.3/"));
        assert!(s.contains("$RNVIM_TOOLS_BIN/fake-ls"));
    }

    #[test]
    fn npm_and_golang_scripts() {
        let pkg = json!({ "name": "pyright", "bin": { "pyright-langserver": "npm:pyright-langserver" },
                          "source": { "id": "pkg:npm/pyright@1.1.0" } });
        let purl = parse_purl("pkg:npm/pyright@1.1.0").unwrap();
        let s = npm_script(&pkg, &purl, "pyright-langserver").unwrap();
        assert!(s.contains("pyright@1.1.0"));
        assert!(s.contains(".bin/pyright-langserver"));

        let pkg = json!({ "name": "gopls", "bin": { "gopls": "golang:gopls" },
                          "source": { "id": "pkg:golang/golang.org/x/tools/gopls@v0.19.1" } });
        let purl = parse_purl("pkg:golang/golang.org/x/tools/gopls@v0.19.1").unwrap();
        let s = golang_script(&pkg, &purl, "gopls").unwrap();
        assert!(s.contains("go install \"golang.org/x/tools/gopls@v0.19.1\""));
    }

    #[test]
    fn finds_by_bin_key() {
        let pkgs = vec![json!({ "name": "pyright", "bin": { "pyright-langserver": "x" } })];
        assert!(find(&pkgs, "pyright").is_some());
        assert!(find(&pkgs, "pyright-langserver").is_some());
        assert!(find(&pkgs, "nope").is_none());
    }
}
