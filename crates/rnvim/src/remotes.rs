//! Remote target management: ssh-config host discovery, recently used
//! workspaces, the pre-launch selector, and the in-editor connect handoff.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecentEntry {
    pub host: String,
    pub path: String,
    pub ts: u64,
}

fn recent_file() -> Result<PathBuf> {
    Ok(crate::nvim::rnvim_home()?.join("recent.json"))
}

pub fn load_recent() -> Vec<RecentEntry> {
    recent_file()
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn record_recent_at(file: &Path, host: &str, path: &str, entries: &mut Vec<RecentEntry>) {
    entries.retain(|e| !(e.host == host && e.path == path));
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    entries.insert(
        0,
        RecentEntry {
            host: host.to_string(),
            path: path.to_string(),
            ts,
        },
    );
    entries.truncate(50);
    if let Some(parent) = file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(
        file,
        serde_json::to_string_pretty(entries).unwrap_or_default(),
    );
}

/// Remember a successfully opened workspace (most recent first).
pub fn record_recent(host: &str, path: &str) {
    if host == "local" {
        return;
    }
    if let Ok(file) = recent_file() {
        let mut entries = load_recent();
        record_recent_at(&file, host, path, &mut entries);
    }
}

/// Host aliases from ~/.ssh/config, following non-glob Include files.
/// Wildcard host patterns (`*`, `?`, `!`) are configuration, not targets.
pub fn ssh_hosts() -> Vec<String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let mut hosts = Vec::new();
    collect_hosts(&home.join(".ssh").join("config"), &home, &mut hosts, 0);
    let mut seen = std::collections::HashSet::new();
    hosts.retain(|h| seen.insert(h.clone()));
    hosts
}

fn collect_hosts(path: &Path, home: &Path, out: &mut Vec<String>, depth: u8) {
    if depth > 3 {
        return;
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let Some(key) = parts.next() else {
            continue;
        };
        match key.to_ascii_lowercase().as_str() {
            "host" => {
                for alias in parts {
                    if !alias.contains(['*', '?', '!']) {
                        out.push(alias.to_string());
                    }
                }
            }
            "include" => {
                for inc in parts {
                    if inc.contains('*') {
                        continue; // glob includes unsupported for now
                    }
                    let p = if let Some(rest) = inc.strip_prefix("~/") {
                        home.join(rest)
                    } else if Path::new(inc).is_absolute() {
                        PathBuf::from(inc)
                    } else {
                        home.join(".ssh").join(inc)
                    };
                    collect_hosts(&p, home, out, depth + 1);
                }
            }
            _ => {}
        }
    }
}

#[derive(Serialize)]
struct TargetsFile<'a> {
    recent: &'a [RecentEntry],
    hosts: &'a [String],
}

/// Materialize the candidate list for the in-editor connect picker.
pub fn write_targets_file() -> Result<PathBuf> {
    let recent = load_recent();
    let hosts = ssh_hosts();
    let dir = crate::nvim::rnvim_home()?.join("run");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("targets-{}.json", std::process::id()));
    std::fs::write(
        &path,
        serde_json::to_string(&TargetsFile {
            recent: &recent,
            hosts: &hosts,
        })?,
    )
    .with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

/// Interactive pre-launch selector for bare `rnvim`. None = local editor.
pub fn select_target() -> Result<Option<String>> {
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        return Ok(None);
    }
    let recent = load_recent();
    let hosts = ssh_hosts();
    if recent.is_empty() && hosts.is_empty() {
        return Ok(None);
    }

    let mut items: Vec<String> = Vec::new();
    for e in &recent {
        items.push(format!("{}:{}", e.host, e.path));
    }
    for h in &hosts {
        if !recent.iter().any(|e| e.host == *h) {
            items.push(h.clone());
        }
    }

    let choice = dialoguer::FuzzySelect::new()
        .with_prompt("rnvim — pick a remote target (Esc for local editor)")
        .items(&items)
        .default(0)
        .interact_opt()
        .context("target selector")?;
    Ok(choice.map(|i| items[i].clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ssh_config_with_includes() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::write(
            home.join(".ssh/config"),
            "# comment\nHost dev-box staging\n  HostName 10.0.0.1\nHost *\n  User me\nInclude extra_config\n",
        )
        .unwrap();
        std::fs::write(home.join(".ssh/extra_config"), "Host gpu-server\n").unwrap();

        let mut hosts = Vec::new();
        collect_hosts(&home.join(".ssh/config"), home, &mut hosts, 0);
        assert_eq!(
            hosts,
            vec!["dev-box", "staging", "gpu-server"],
            "wildcards skipped"
        );
    }

    #[test]
    fn recent_dedupes_and_orders() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("recent.json");
        let mut entries = Vec::new();
        record_recent_at(&file, "a", "/x", &mut entries);
        record_recent_at(&file, "b", "/y", &mut entries);
        record_recent_at(&file, "a", "/x", &mut entries); // revisit → moves to front

        let text = std::fs::read_to_string(&file).unwrap();
        let loaded: Vec<RecentEntry> = serde_json::from_str(&text).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(
            (loaded[0].host.as_str(), loaded[0].path.as_str()),
            ("a", "/x")
        );
        assert_eq!(loaded[1].host.as_str(), "b");
    }
}
