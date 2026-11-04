//! Workspace sandbox: every path a tool touches is resolved against the
//! workspace root and rejected if it escapes it (absolute paths, `..`,
//! symlinks pointing outside).

use anyhow::{anyhow, Result};
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Workspace {
    root: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
}

impl Workspace {
    pub fn new(root: impl AsRef<Path>) -> Result<Self> {
        let root = root
            .as_ref()
            .canonicalize()
            .map_err(|e| anyhow!("Cannot open workspace root {}: {}", root.as_ref().display(), e))?;
        Ok(Self { root })
    }

    pub fn current_dir() -> Result<Self> {
        Self::new(std::env::current_dir()?)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Path relative to the root, for display.
    pub fn display(&self, p: &Path) -> String {
        p.strip_prefix(&self.root)
            .map(|r| {
                let s = r.display().to_string();
                if s.is_empty() { ".".to_string() } else { s }
            })
            .unwrap_or_else(|_| p.display().to_string())
    }

    /// Resolve `input` to an absolute path inside the workspace.
    pub fn resolve(&self, input: &str, access: Access) -> Result<PathBuf> {
        let input = input.trim();
        if input.is_empty() {
            return Err(anyhow!("Empty path"));
        }
        let expanded = if let Some(rest) = input.strip_prefix("~/") {
            dirs::home_dir().ok_or_else(|| anyhow!("no home dir"))?.join(rest)
        } else {
            PathBuf::from(input)
        };
        let joined = if expanded.is_absolute() { expanded } else { self.root.join(expanded) };

        // Canonicalize the longest existing ancestor (follows symlinks), then
        // re-append the not-yet-existing tail, refusing `..` in the tail.
        let mut existing = joined.clone();
        let mut tail: Vec<std::ffi::OsString> = Vec::new();
        while !existing.exists() {
            match existing.file_name() {
                Some(name) => {
                    tail.push(name.to_os_string());
                    if !existing.pop() {
                        break;
                    }
                }
                None => {
                    // path ends with `..` or root component
                    return Err(self.outside(input));
                }
            }
        }
        let mut resolved = existing
            .canonicalize()
            .map_err(|e| anyhow!("Cannot resolve {}: {}", input, e))?;
        for part in tail.iter().rev() {
            let p = Path::new(part);
            if p.components().any(|c| !matches!(c, Component::Normal(_))) {
                return Err(self.outside(input));
            }
            resolved.push(part);
        }

        if !resolved.starts_with(&self.root) {
            return Err(self.outside(input));
        }

        if access == Access::Write {
            let rel = resolved.strip_prefix(&self.root).unwrap_or(&resolved);
            if rel.components().any(|c| c.as_os_str() == ".git") {
                return Err(anyhow!("PermissionDenied: writing inside .git/ is not allowed ({})", input));
            }
        }
        Ok(resolved)
    }

    fn outside(&self, input: &str) -> anyhow::Error {
        anyhow!(
            "PathOutsideWorkspace: '{}' resolves outside the workspace root {}. Only paths inside the workspace are allowed.",
            input,
            self.root.display()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_escape_attempts() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "x").unwrap();
        let ws = Workspace::new(dir.path()).unwrap();
        assert!(ws.resolve("a.txt", Access::Read).is_ok());
        assert!(ws.resolve("new/dir/b.txt", Access::Write).is_ok());
        for bad in ["../../etc/passwd", "/etc/hostname", "/etc/passwd", "new/../../x", "~/.ssh/id_rsa"] {
            let r = ws.resolve(bad, Access::Read);
            assert!(r.is_err(), "{bad} should be rejected");
            assert!(r.unwrap_err().to_string().contains("PathOutsideWorkspace"));
        }
        assert!(ws.resolve(".git/config", Access::Write).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_out() {
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("/etc", dir.path().join("etc_link")).unwrap();
        let ws = Workspace::new(dir.path()).unwrap();
        assert!(ws.resolve("etc_link/hostname", Access::Read).is_err());
        assert!(ws.resolve("etc_link/newfile", Access::Write).is_err());
    }
}
