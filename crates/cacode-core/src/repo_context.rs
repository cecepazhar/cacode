//! Repository context injected into the system prompt: cwd, git state,
//! ignore-aware tree, project memory file and a compact symbol map.

use crate::symbols::SymbolIndex;
use crate::tools::search::tree;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

pub const MEMORY_FILES: &[&str] = &["CACODE.md", "AGENTS.md", "CLAUDE.md"];
const MEMORY_MAX_BYTES: usize = 8 * 1024;

fn git(root: &Path, args: &[&str]) -> Option<String> {
    let mut child = Command::new("git")
        .args(args)
        .current_dir(root)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    // git on a huge repo must never stall prompt building
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        match child.try_wait() {
            Ok(Some(st)) if st.success() => break,
            Ok(Some(_)) => return None,
            Ok(None) if std::time::Instant::now() > deadline => {
                let _ = child.kill();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(15)),
            Err(_) => return None,
        }
    }
    let mut out = String::new();
    use std::io::Read;
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    Some(out.trim_end().to_string())
}

pub struct RepoContextOptions {
    pub tree_depth: usize,
    pub tree_entries: usize,
    pub map_budget: usize,
}

impl Default for RepoContextOptions {
    fn default() -> Self {
        Self { tree_depth: 2, tree_entries: 150, map_budget: 3_000 }
    }
}

/// Build the `<repo_context>` block. Cheap enough to rebuild every turn.
pub fn build(root: &Path, symbols: Option<&SymbolIndex>, opts: &RepoContextOptions) -> String {
    let mut s = String::from("<repo_context>\n");
    s.push_str(&format!("Workspace root: {}\n", root.display()));
    s.push_str(&format!("Platform: {} {}\nDate: {}\n", std::env::consts::OS, std::env::consts::ARCH, chrono::Local::now().format("%Y-%m-%d")));
    match git(root, &["rev-parse", "--abbrev-ref", "HEAD"]) {
        Some(branch) => {
            s.push_str(&format!("Git branch: {}\n", branch));
            let status = git(root, &["status", "--short"]).unwrap_or_default();
            let lines: Vec<&str> = status.lines().collect();
            if lines.is_empty() {
                s.push_str("Git status: clean\n");
            } else {
                s.push_str(&format!("Git status ({} changed):\n", lines.len()));
                for l in lines.iter().take(30) {
                    s.push_str(&format!("  {}\n", l));
                }
                if lines.len() > 30 {
                    s.push_str("  …\n");
                }
            }
            if let Some(log) = git(root, &["log", "--oneline", "-5"]) {
                s.push_str(&format!("Recent commits:\n{}\n", log.lines().map(|l| format!("  {}", l)).collect::<Vec<_>>().join("\n")));
            }
        }
        None => s.push_str("Git: not a repository\n"),
    }
    s.push_str(&format!("\nFile tree (depth {}, ignore-aware):\n{}\n", opts.tree_depth, tree(root, root, opts.tree_depth, opts.tree_entries)));

    for f in MEMORY_FILES {
        let p = root.join(f);
        if let Ok(text) = std::fs::read_to_string(&p) {
            s.push_str(&format!("\nProject instructions from {} (follow them):\n{}\n", f, crate::text::prefix_bytes(&text, MEMORY_MAX_BYTES)));
            break;
        }
    }
    if let Some(idx) = symbols {
        let map = idx.repo_map(opts.map_budget);
        if !map.is_empty() {
            s.push_str(&format!("\nRepo map (key symbols per file; use find_symbol / read_file for detail):\n{}", map));
        }
    }
    s.push_str("</repo_context>");
    s
}
