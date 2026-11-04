//! File tools: read (paginated, numbered), write (atomic + stale check),
//! edit (safe search/replace) and undo.

use super::{ToolContext, Approval};
use crate::events::ApprovalRequest;
use crate::permission::ActionKind;
use crate::text::{looks_binary, truncate_chars};
use crate::tools::workspace::Access;
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub const READ_DEFAULT_LIMIT: usize = 2000;
pub const READ_MAX_BYTES: usize = 256 * 1024;
pub const READ_MAX_LINE_CHARS: usize = 2000;

#[derive(Debug, Clone)]
pub struct FileStamp {
    pub sha256: String,
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn arg_str<'a>(args: &'a serde_json::Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| args.get(*k).and_then(|v| v.as_str()))
}

pub fn read_file(ctx: &ToolContext, args: &serde_json::Value) -> Result<String> {
    let raw = arg_str(args, &["path", "file_path"]).ok_or_else(|| anyhow!("Missing path"))?;
    let path = ctx.workspace.resolve(raw, Access::Read)?;
    if path.is_dir() {
        return Err(anyhow!("{} is a directory; use list_dir or glob", raw));
    }
    let bytes = std::fs::read(&path).map_err(|e| anyhow!("Cannot read {}: {}", raw, e))?;
    if looks_binary(&bytes) {
        return Err(anyhow!("{} looks like a binary file ({} bytes); refusing to read it as text", raw, bytes.len()));
    }
    let text = String::from_utf8_lossy(&bytes);
    ctx.record_read(&path, &bytes);

    let offset = args.get("offset").and_then(|v| v.as_u64()).unwrap_or(1).max(1) as usize;
    let limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .unwrap_or(READ_DEFAULT_LIMIT)
        .clamp(1, READ_DEFAULT_LIMIT);

    let total = text.lines().count();
    if total == 0 {
        return Ok(format!("[{}] (empty file)", ctx.workspace.display(&path)));
    }
    if offset > total {
        return Err(anyhow!("offset {} is past the end of {} ({} lines)", offset, raw, total));
    }

    let mut out = String::new();
    let mut last = offset - 1;
    for (i, line) in text.lines().enumerate().skip(offset - 1).take(limit) {
        let shown = if line.chars().count() > READ_MAX_LINE_CHARS { truncate_chars(line, READ_MAX_LINE_CHARS) } else { line.to_string() };
        let entry = format!("{:>6}\t{}\n", i + 1, shown);
        if out.len() + entry.len() > READ_MAX_BYTES {
            break;
        }
        out.push_str(&entry);
        last = i + 1;
    }
    let mut header = format!("[{}] lines {}-{} of {}", ctx.workspace.display(&path), offset, last, total);
    if last < total {
        header.push_str(&format!(" — more available, call read_file with offset={}", last + 1));
    }
    Ok(format!("{}\n{}", header, out.trim_end_matches('\n')))
}

/// Checks the file was read in this session and has not changed since.
fn check_fresh(ctx: &ToolContext, path: &Path, display: &str) -> Result<Option<Vec<u8>>> {
    if !path.exists() {
        return Ok(None);
    }
    let current = std::fs::read(path)?;
    match ctx.stamp(path) {
        None => Err(anyhow!(
            "NotReadYet: {} exists but has not been read in this session. Call read_file on it first so you edit the current content.",
            display
        )),
        Some(stamp) if stamp.sha256 != sha256_hex(&current) => Err(anyhow!(
            "StaleFile: {} changed on disk since you last read it (edited by the user or another process). Read it again before editing.",
            display
        )),
        Some(_) => Ok(Some(current)),
    }
}

pub fn unified_diff(display: &str, old: &str, new: &str) -> String {
    similar::TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(3)
        .header(&format!("a/{}", display), &format!("b/{}", display))
        .to_string()
}

/// Atomic write: temp file in the same directory, fsync, rename. Keeps the
/// original permissions when replacing an existing file.
pub fn atomic_write(path: &Path, content: &[u8]) -> Result<()> {
    use std::io::Write;
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let fname = path.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_else(|| "file".into());
    let tmp = parent.join(format!(".{}.cacode-tmp-{}", fname, std::process::id()));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(content)?;
        f.sync_all()?;
    }
    if let Ok(meta) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(&tmp, meta.permissions());
    }
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        anyhow!("rename failed for {}: {}", path.display(), e)
    })
}

/// Shared commit path for write/edit: approval, lock, undo snapshot, atomic write.
async fn commit_edit(ctx: &ToolContext, tool: &str, path: PathBuf, old: Option<Vec<u8>>, new: String) -> Result<String> {
    let display = ctx.workspace.display(&path);
    let old_text = old.as_ref().map(|b| String::from_utf8_lossy(b).to_string()).unwrap_or_default();
    if old.is_some() && old_text == new {
        return Ok(format!("No changes: {} already has the requested content", display));
    }
    let diff = unified_diff(&display, &old_text, &new);
    let summary = if old.is_some() { format!("Edit {}", display) } else { format!("Create {}", display) };

    match ctx
        .authorize(ActionKind::Edit, false, ApprovalRequest { id: 0, tool: tool.into(), summary: summary.clone(), diff: Some(diff.clone()) })
        .await?
    {
        Approval::Granted => {}
        Approval::Denied(reason) => return Err(anyhow!("Rejected: {} — {}", summary, reason)),
    }

    let _lock = ctx.lock_path(&path)?;
    // Re-check freshness after (possibly long) approval wait.
    if let Some(prev) = &old {
        let now = std::fs::read(&path).unwrap_or_default();
        if &now != prev {
            return Err(anyhow!("StaleFile: {} changed while waiting for approval; re-read it", display));
        }
    }
    ctx.undo.push(&path, old.as_deref())?;
    atomic_write(&path, new.as_bytes())?;
    ctx.record_read(&path, new.as_bytes());
    ctx.touch(&display);

    let (adds, dels) = diff.lines().fold((0, 0), |(a, d), l| {
        if l.starts_with('+') && !l.starts_with("+++") { (a + 1, d) } else if l.starts_with('-') && !l.starts_with("---") { (a, d + 1) } else { (a, d) }
    });
    Ok(format!(
        "{} ({} bytes, +{} -{}). sha256={}\n{}",
        if old.is_some() { format!("Updated {}", display) } else { format!("Created {}", display) },
        new.len(),
        adds,
        dels,
        &sha256_hex(new.as_bytes())[..12],
        crate::text::clamp_output(&diff, 8_000, 200)
    ))
}

pub async fn write_file(ctx: &ToolContext, args: &serde_json::Value) -> Result<String> {
    let raw = arg_str(args, &["path", "file_path"]).ok_or_else(|| anyhow!("Missing path"))?;
    let content = arg_str(args, &["content"]).ok_or_else(|| anyhow!("Missing content"))?;
    let path = ctx.workspace.resolve(raw, Access::Write)?;
    let display = ctx.workspace.display(&path);
    let old = check_fresh(ctx, &path, &display)?;
    commit_edit(ctx, "write_file", path, old, content.to_string()).await
}

pub async fn edit_file(ctx: &ToolContext, tool: &str, args: &serde_json::Value) -> Result<String> {
    let raw = arg_str(args, &["path", "file_path"]).ok_or_else(|| anyhow!("Missing path"))?;
    let search = arg_str(args, &["old_string", "search"]).ok_or_else(|| anyhow!("Missing old_string"))?;
    let replace = arg_str(args, &["new_string", "replace"]).ok_or_else(|| anyhow!("Missing new_string"))?;
    let replace_all = args.get("replace_all").and_then(|v| v.as_bool()).unwrap_or(false);
    let path = ctx.workspace.resolve(raw, Access::Write)?;
    let display = ctx.workspace.display(&path);

    let old = check_fresh(ctx, &path, &display)?;
    let old_text = match &old {
        Some(b) => String::from_utf8(b.clone()).map_err(|_| anyhow!("{} is not valid UTF-8", display))?,
        None if search.is_empty() => String::new(),
        None => return Err(anyhow!("File does not exist: {} (use write_file to create it)", display)),
    };
    let new = crate::search_replace::apply_search_replace_opts(&old_text, search, replace, replace_all)?;
    commit_edit(ctx, tool, path, old, new).await
}

// ---------------------------------------------------------------- undo ----

#[derive(Serialize, Deserialize)]
struct UndoEntry {
    path: PathBuf,
    /// base64 of the previous content; None = file did not exist.
    prev_b64: Option<String>,
    ts: String,
}

/// Persistent undo journal at `~/.cacode/undo/<workspace-hash>/`.
#[derive(Debug)]
pub struct UndoStore {
    dir: PathBuf,
}

impl UndoStore {
    pub fn for_workspace(root: &Path) -> Self {
        let base = dirs::home_dir().unwrap_or_else(std::env::temp_dir).join(".cacode").join("undo");
        Self { dir: base.join(&sha256_hex(root.to_string_lossy().as_bytes())[..16]) }
    }

    pub fn at(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn push(&self, path: &Path, prev: Option<&[u8]>) -> Result<()> {
        use base64::Engine;
        std::fs::create_dir_all(&self.dir)?;
        let entry = UndoEntry {
            path: path.to_path_buf(),
            prev_b64: prev.map(|b| base64::engine::general_purpose::STANDARD.encode(b)),
            ts: chrono::Utc::now().to_rfc3339(),
        };
        let name = format!("{}-{:09}.json", chrono::Utc::now().format("%Y%m%d%H%M%S%f"), std::process::id());
        std::fs::write(self.dir.join(name), serde_json::to_vec(&entry)?)?;
        Ok(())
    }

    fn entries(&self) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = std::fs::read_dir(&self.dir)
            .map(|rd| rd.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().map(|x| x == "json").unwrap_or(false)).collect())
            .unwrap_or_default();
        v.sort();
        v
    }

    pub fn len(&self) -> usize {
        self.entries().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Revert the most recent edit. Returns a description.
    pub fn undo_last(&self) -> Result<String> {
        use base64::Engine;
        let last = self.entries().pop().ok_or_else(|| anyhow!("Nothing to undo"))?;
        let entry: UndoEntry = serde_json::from_slice(&std::fs::read(&last)?)?;
        let msg = match entry.prev_b64 {
            Some(b64) => {
                let bytes = base64::engine::general_purpose::STANDARD.decode(b64)?;
                atomic_write(&entry.path, &bytes)?;
                format!("Restored {} to its state before the edit at {}", entry.path.display(), entry.ts)
            }
            None => {
                if entry.path.exists() {
                    std::fs::remove_file(&entry.path)?;
                }
                format!("Removed {} (it was created by the undone edit)", entry.path.display())
            }
        };
        std::fs::remove_file(last)?;
        Ok(msg)
    }
}
