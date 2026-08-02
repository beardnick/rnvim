//! Remote-side workspace search: fuzzy file finding (nucleo) and content
//! grep (ripgrep's engine as libraries). Only top-N results cross the wire.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use grep_matcher::Matcher as _;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::sinks::UTF8;
use grep_searcher::SearcherBuilder;
use ignore::WalkBuilder;
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher};
use rnvim_proto::*;
use serde_json::{json, Value};

/// Hard cap on walked files, so a pathological root can't wedge the agent.
const MAX_FILES: usize = 200_000;
/// File lists are cached per root; queries within the TTL reuse the walk.
const CACHE_TTL: Duration = Duration::from_secs(10);

type FileCache = Mutex<HashMap<PathBuf, (Instant, Arc<Vec<String>>)>>;

fn cache() -> &'static FileCache {
    static CACHE: OnceLock<FileCache> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// Walk `root` honoring .gitignore/hidden rules, newest cache wins.
fn list_files(root: &Path) -> Arc<Vec<String>> {
    {
        let cache = cache().lock().unwrap();
        if let Some((at, files)) = cache.get(root) {
            if at.elapsed() < CACHE_TTL {
                return Arc::clone(files);
            }
        }
    }

    let mut files = Vec::new();
    for entry in WalkBuilder::new(root).build().flatten() {
        if files.len() >= MAX_FILES {
            break;
        }
        if entry.file_type().is_some_and(|t| t.is_file()) {
            if let Ok(rel) = entry.path().strip_prefix(root) {
                files.push(rel.to_string_lossy().into_owned());
            }
        }
    }
    files.sort();
    let files = Arc::new(files);
    cache()
        .lock()
        .unwrap()
        .insert(root.to_path_buf(), (Instant::now(), Arc::clone(&files)));
    files
}

pub fn find_files(p: FindFilesParams) -> Result<Value> {
    let root = crate::expand(&p.root);
    let limit = p.limit.unwrap_or(100).min(1000) as usize;
    let files = list_files(&root);

    let out: Vec<String> = if p.query.trim().is_empty() {
        files.iter().take(limit).cloned().collect()
    } else {
        let mut matcher = Matcher::new(Config::DEFAULT.match_paths());
        let pattern = Pattern::parse(&p.query, CaseMatching::Smart, Normalization::Smart);
        pattern
            .match_list(files.iter(), &mut matcher)
            .into_iter()
            .take(limit)
            .map(|(f, _score)| f.clone())
            .collect()
    };

    Ok(json!(FindFilesResult {
        files: out,
        total: files.len() as u64
    }))
}

fn escape_regex(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for c in s.chars() {
        if !c.is_alphanumeric() && c != '_' {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn build_matcher(query: &str) -> Result<RegexMatcher> {
    RegexMatcherBuilder::new()
        .case_smart(true)
        .build(query)
        .or_else(|_| {
            // Not a valid regex: search for it literally.
            RegexMatcherBuilder::new()
                .case_smart(true)
                .build(&escape_regex(query))
        })
        .context("build search pattern")
}

pub fn find_grep(p: GrepParams) -> Result<Value> {
    let root = crate::expand(&p.root);
    let limit = p.limit.unwrap_or(200).min(2000) as usize;
    if p.query.trim().is_empty() {
        return Ok(json!(GrepResult {
            matches: Vec::new(),
            truncated: false
        }));
    }

    let matcher = build_matcher(&p.query)?;
    let mut searcher = SearcherBuilder::new().line_number(true).build();
    let mut matches: Vec<GrepMatch> = Vec::new();

    for entry in WalkBuilder::new(&root).build().flatten() {
        if matches.len() >= limit {
            break;
        }
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.path();
        let rel = path
            .strip_prefix(&root)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned();
        // Binary files and IO errors just skip this file.
        let _ = searcher.search_path(
            &matcher,
            path,
            UTF8(|line_num, line| {
                let col = matcher
                    .find(line.as_bytes())
                    .ok()
                    .flatten()
                    .map(|m| m.start() + 1)
                    .unwrap_or(1);
                matches.push(GrepMatch {
                    path: rel.clone(),
                    line: line_num,
                    col: col as u64,
                    text: line.trim_end().chars().take(300).collect(),
                });
                Ok(matches.len() < limit)
            }),
        );
    }

    let truncated = matches.len() >= limit;
    Ok(json!(GrepResult { matches, truncated }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("target")).unwrap();
        fs::create_dir(root.join(".git")).unwrap(); // make ignore rules apply
        fs::write(root.join(".gitignore"), "/target\n").unwrap();
        fs::write(
            root.join("src/main.rs"),
            "fn main() {\n    println!(\"hi\");\n}\n",
        )
        .unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn add(a: i32) -> i32 { a }\n").unwrap();
        fs::write(root.join("README.md"), "# demo\n").unwrap();
        fs::write(root.join("target/junk.txt"), "ignored\n").unwrap();
        dir
    }

    #[test]
    fn fuzzy_find_ranks_and_ignores() {
        let dir = fixture();
        let res = find_files(FindFilesParams {
            root: dir.path().to_string_lossy().into_owned(),
            query: "mainrs".into(),
            limit: Some(10),
        })
        .unwrap();
        let r: FindFilesResult = serde_json::from_value(res).unwrap();
        assert_eq!(r.files.first().map(String::as_str), Some("src/main.rs"));
        assert!(
            !r.files.iter().any(|f| f.starts_with("target/")),
            "gitignored files must not appear: {:?}",
            r.files
        );
    }

    #[test]
    fn empty_query_lists_files() {
        let dir = fixture();
        let res = find_files(FindFilesParams {
            root: dir.path().to_string_lossy().into_owned(),
            query: "".into(),
            limit: Some(100),
        })
        .unwrap();
        let r: FindFilesResult = serde_json::from_value(res).unwrap();
        // 3 tracked files; .gitignore (hidden) and target/ (ignored) excluded
        assert_eq!(r.total, 3, "{:?}", r.files);
    }

    #[test]
    fn grep_finds_lines_with_positions() {
        let dir = fixture();
        let res = find_grep(GrepParams {
            root: dir.path().to_string_lossy().into_owned(),
            query: "fn main".into(),
            limit: Some(10),
        })
        .unwrap();
        let r: GrepResult = serde_json::from_value(res).unwrap();
        assert_eq!(r.matches.len(), 1);
        let m = &r.matches[0];
        assert_eq!((m.path.as_str(), m.line, m.col), ("src/main.rs", 1, 1));
        assert!(m.text.contains("fn main"));
    }

    #[test]
    fn grep_invalid_regex_falls_back_to_literal() {
        let dir = fixture();
        fs::write(dir.path().join("weird.txt"), "a(b\n").unwrap();
        let res = find_grep(GrepParams {
            root: dir.path().to_string_lossy().into_owned(),
            query: "a(b".into(),
            limit: Some(10),
        })
        .unwrap();
        let r: GrepResult = serde_json::from_value(res).unwrap();
        assert_eq!(r.matches.len(), 1);
        assert_eq!(r.matches[0].path, "weird.txt");
    }
}
