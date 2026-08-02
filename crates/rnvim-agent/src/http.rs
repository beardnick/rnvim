//! Native HTTP download — no shelling out to curl. Pure-Rust TLS (rustls)
//! so the static musl agent carries it everywhere; honors proxy settings
//! from the environment; atomic .part → rename on completion.

use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};

pub fn download(url: &str, dest: &Path) -> Result<u64> {
    let agent = ureq::AgentBuilder::new()
        .try_proxy_from_env(true)
        .timeout_connect(Duration::from_secs(30))
        .build();
    let resp = agent
        .get(url)
        .call()
        .with_context(|| format!("GET {url}"))?;
    if resp.status() != 200 {
        bail!("GET {url}: HTTP {}", resp.status());
    }

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let part = dest.with_extension("part");
    let mut file =
        std::fs::File::create(&part).with_context(|| format!("create {}", part.display()))?;
    let mut reader = resp.into_reader();
    let bytes = std::io::copy(&mut reader, &mut file).map_err(|e| {
        let _ = std::fs::remove_file(&part);
        anyhow::anyhow!("download interrupted: {e}")
    })?;
    std::fs::rename(&part, dest)?;
    Ok(bytes)
}
