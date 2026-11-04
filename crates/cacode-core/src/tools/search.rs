//! Search & navigation tools, ignore-aware (.gitignore, .ignore, .cacodeignore
//! plus a built-in deny list for build output, vendored deps and binaries).

use super::ToolContext;
use crate::text::{looks_binary, truncate_chars};
use crate::tools::workspace::Access;
use anyhow::{anyhow, Result};
use ignore::WalkBuilder;
use std::path::{Path, PathBuf};

/// Directories never descended into, even without an ignore file.
pub const ALWAYS_IGNORED_DIRS: &[&str] = &[
    ".git", "target", "node_modules", ".venv", "venv", "__pycache__", "dist", "build", ".next", ".cache", "bin", "obj", ".idea", ".gradle",
];
pub const MAX_FILE_BYTES: u64 = 1024 * 1024;
pub const IGNORE_FILE: &str = ".cacodeignore";

/// Ignore-aware walker rooted at `start`.
pub fn walker(start: &Path, max_depth: Option<usize>) -> ignore::Walk {
    let mut b = WalkBuilder::new(start);
    b.hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .ignore(true)
        .require_git(false)
        .add_custom_ignore_filename(IGNORE_FILE)
        .max_depth(max_depth)
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            !(e.file_type().map(|t| t.is_dir()).unwrap_or(false) && ALWAYS_IGNORED_DIRS.contains(&name.as_ref()))
        });
    b.build()
}

/// All indexable text files under `start`.
pub fn text_files(start: &Path) -> Vec<PathBuf> {
    walker(start, None)
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .filter(|e| e.metadata().map(|m| m.len() <= MAX_FILE_BYTES).unwrap_or(false))
        .map(|e| e.into_path())
        .collect()
}

fn build_glob(pattern: &str) -> Result<globset::GlobMatcher> {
    let pat = if pattern.contains('/') || pattern.starts_with("**") { pattern.to_string() } else { format!("**/{}", pattern) };
    Ok(globset::GlobBuilder::new(&pat).literal_separator(false).build()?.compile_matcher())
}

pub fn grep(ctx: &ToolContext, args: &serde_json::Value) -> Result<String> {
    let pattern = args.get("pattern").and_then(|v| v.as_str()).ok_or_else(|| anyhow!("Missing pattern"))?;
    let dir = args.get("path").or_else(|| args.get("dir")).and_then(|v| v.as_str()).unwrap_or(".");
    let case_i = args.get("case_insensitive").or_else(|| args.get("-i")).and_then(|v| v.as_bool()).unwrap_or(false);
    let mode = args.get("output_mode").and_then(|v| v.as_str()).unwrap_or("content");
    let head_limit = args.get("head_limit").and_then(|v| v.as_u64()).unwrap_or(200) as usize;
    let glob = args.get("glob").and_then(|v| v.as_str()).map(build_glob).transpose()?;

    let re = regex::RegexBuilder::new(pattern).case_insensitive(case_i).size_limit(10 << 20).build()?;
    let root = ctx.workspace.resolve(dir, Access::Read)?;
    let files: Vec<PathBuf> = if root.is_file() { vec![root.clone()] } else { text_files(&root) };

    let mut lines_out = Vec::new();
    let mut files_hit = 0usize;
    let mut total_hits = 0usize;
    let mut truncated = false;
    for f in files {
        let rel = ctx.workspace.display(&f);
        if let Some(g) = &glob {
            if !g.is_match(&rel) && !g.is_match(f.file_name().unwrap_or_default()) {
                continue;
            }
        }
        let Ok(bytes) = std::fs::read(&f) else { continue };
        if looks_binary(&bytes) {
            continue;
        }
        let text = String::from_utf8_lossy(&bytes);
        let mut count = 0usize;
        for (i, line) in text.lines().enumerate() {
            if re.is_match(line) {
                count += 1;
                if mode == "content" {
                    if lines_out.len() >= head_limit {
                        truncated = true;
                        break;
                    }
                    lines_out.push(format!("{}:{}:{}", rel, i + 1, truncate_chars(line.trim_end(), 300)));
                }
            }
        }
        if count > 0 {
            files_hit += 1;
            total_hits += count;
            match mode {
                "files_with_matches" => lines_out.push(rel),
                "count" => lines_out.push(format!("{}:{}", rel, count)),
                _ => {}
            }
        }
        if truncated || (mode != "content" && lines_out.len() >= head_limit) {
            truncated = true;
            break;
        }
    }
    if lines_out.is_empty() {
        return Ok(format!("No matches for /{}/ in {}", pattern, ctx.workspace.display(&root)));
    }
    let mut s = lines_out.join("\n");
    s.push_str(&format!("\n[{} matches in {} files{}]", total_hits, files_hit, if truncated { "; output limited — narrow with path/glob or raise head_limit" } else { "" }));
    Ok(s)
}

pub fn glob(ctx: &ToolContext, args: &serde_json::Value) -> Result<String> {
    let pattern = args.get("pattern").and_then(|v| v.as_str()).ok_or_else(|| anyhow!("Missing pattern"))?;
    let dir = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(200) as usize;
    let root = ctx.workspace.resolve(dir, Access::Read)?;
    let m = build_glob(pattern)?;
    let mut hits: Vec<(std::time::SystemTime, String)> = walker(&root, None)
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .filter_map(|e| {
            let rel_root = e.path().strip_prefix(&root).ok()?.to_string_lossy().to_string();
            if m.is_match(&rel_root) {
                let mt = e.metadata().ok()?.modified().ok()?;
                Some((mt, ctx.workspace.display(e.path())))
            } else {
                None
            }
        })
        .collect();
    hits.sort_by_key(|h| std::cmp::Reverse(h.0));
    let total = hits.len();
    if total == 0 {
        return Ok(format!("No files match {}", pattern));
    }
    let mut out: Vec<String> = hits.into_iter().take(limit).map(|h| h.1).collect();
    if total > limit {
        out.push(format!("[{} more files not shown]", total - limit));
    }
    Ok(out.join("\n"))
}

/// Ignore-aware tree listing.
pub fn tree(root: &Path, display_root: &Path, depth: usize, max_entries: usize) -> String {
    let mut entries: Vec<(PathBuf, bool)> = walker(root, Some(depth))
        .filter_map(|e| e.ok())
        .filter(|e| e.depth() > 0)
        .map(|e| (e.path().to_path_buf(), e.file_type().map(|t| t.is_dir()).unwrap_or(false)))
        .collect();
    entries.sort();
    let total = entries.len();
    let mut out = Vec::new();
    for (p, is_dir) in entries.into_iter().take(max_entries) {
        let rel = p.strip_prefix(display_root).unwrap_or(&p);
        let d = rel.components().count().saturating_sub(1);
        let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        out.push(format!("{}{}{}", "  ".repeat(d), name, if is_dir { "/" } else { "" }));
    }
    if total > max_entries {
        out.push(format!("… ({} more entries)", total - max_entries));
    }
    out.join("\n")
}

pub fn list_dir(ctx: &ToolContext, args: &serde_json::Value) -> Result<String> {
    let dir = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
    let depth = args.get("depth").and_then(|v| v.as_u64()).unwrap_or(2).clamp(1, 6) as usize;
    let root = ctx.workspace.resolve(dir, Access::Read)?;
    if !root.is_dir() {
        return Err(anyhow!("{} is not a directory", dir));
    }
    Ok(format!("{}/\n{}", ctx.workspace.display(&root), tree(&root, &root, depth, 400)))
}
