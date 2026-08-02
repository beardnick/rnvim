//! The Lua runtime shipped inside the rnvim binary, extracted to
//! ~/.rnvim/runtime/<version>/ at launch. Always re-extracted: it is tiny,
//! and this guarantees the on-disk runtime matches the running binary.

use std::path::PathBuf;

use anyhow::{Context, Result};
use include_dir::{include_dir, Dir};

static RUNTIME: Dir = include_dir!("$CARGO_MANIFEST_DIR/../../runtime");

pub fn ensure_runtime() -> Result<PathBuf> {
    let dst = crate::nvim::rnvim_home()?
        .join("runtime")
        .join(env!("CARGO_PKG_VERSION"));
    std::fs::create_dir_all(&dst)?;
    RUNTIME
        .extract(&dst)
        .with_context(|| format!("extract runtime to {}", dst.display()))?;
    Ok(dst)
}
