//! Workspace trust. Project-level hooks and MCP servers execute commands, so
//! they are only loaded from workspaces the user explicitly trusted
//! (`/trust` in the TUI, `--trust-workspace` headless). Global config in
//! `~/.cacode/` is always trusted.

use std::path::{Path, PathBuf};

fn store() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".cacode").join("trusted_workspaces.json"))
}

fn load() -> Vec<PathBuf> {
    store().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

pub fn is_trusted(root: &Path) -> bool {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    load().iter().any(|p| root.starts_with(p))
}

pub fn trust(root: &Path) -> anyhow::Result<()> {
    let root = root.canonicalize()?;
    let mut list = load();
    if !list.contains(&root) {
        list.push(root);
    }
    let p = store().ok_or_else(|| anyhow::anyhow!("no home directory"))?;
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(p, serde_json::to_vec_pretty(&list)?)?;
    Ok(())
}

/// Project files that would be ignored in an untrusted workspace.
pub fn project_exec_files(root: &Path) -> Vec<String> {
    [".mcp.json", ".cacode/mcp.json", ".cacode/hooks.toml"].iter().filter(|f| root.join(f).exists()).map(|f| f.to_string()).collect()
}
