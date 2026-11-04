//! Persistent sessions (`~/.cacode/sessions/<id>.json`) for `--continue`,
//! `--resume <id>` and the TUI `/resume` command.

use crate::types::{Message, TokenUsage};
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionRecord {
    pub id: String,
    pub title: String,
    pub workspace: PathBuf,
    pub created: String,
    pub updated: String,
    pub provider: String,
    pub history: Vec<Message>,
    #[serde(default)]
    pub total_usage: TokenUsage,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionSummary {
    pub id: String,
    pub title: String,
    pub workspace: PathBuf,
    pub updated: String,
    pub messages: usize,
}

pub fn new_id() -> String {
    let mut b = [0u8; 4];
    let _ = getrandom::getrandom(&mut b);
    format!("{}-{}", chrono::Local::now().format("%Y%m%d-%H%M%S"), hex::encode(b))
}

pub struct SessionStore {
    dir: PathBuf,
}

impl SessionStore {
    pub fn default_dir() -> PathBuf {
        dirs::home_dir().unwrap_or_else(std::env::temp_dir).join(".cacode").join("sessions")
    }

    pub fn open_default() -> Self {
        Self { dir: Self::default_dir() }
    }

    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    fn path(&self, id: &str) -> Result<PathBuf> {
        if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            return Err(anyhow!("invalid session id '{}'", id));
        }
        Ok(self.dir.join(format!("{}.json", id)))
    }

    /// Title = first user message (trimmed), for listings.
    pub fn title_from(history: &[Message]) -> String {
        history
            .iter()
            .find(|m| m.role == "user" && !m.content.starts_with("[COMPACTED") && !m.content.contains("(The conversation above this point was compacted"))
            .map(|m| crate::text::truncate_chars(m.content.lines().next().unwrap_or("").trim(), 70))
            .unwrap_or_else(|| "(empty)".into())
    }

    pub fn save(&self, rec: &SessionRecord) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let p = self.path(&rec.id)?;
        let tmp = p.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(rec)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
        std::fs::rename(tmp, p)?;
        Ok(())
    }

    pub fn load(&self, id: &str) -> Result<SessionRecord> {
        let p = self.path(id)?;
        let bytes = std::fs::read(&p).map_err(|_| anyhow!("session '{}' not found in {}", id, self.dir.display()))?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    /// Newest first; optionally only sessions for one workspace.
    pub fn list(&self, workspace: Option<&Path>) -> Vec<SessionSummary> {
        let mut out: Vec<SessionSummary> = std::fs::read_dir(&self.dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .filter(|e| e.path().extension().map(|x| x == "json").unwrap_or(false))
                    .filter_map(|e| std::fs::read(e.path()).ok())
                    .filter_map(|b| serde_json::from_slice::<SessionRecord>(&b).ok())
                    .filter(|r| workspace.map(|w| r.workspace == w).unwrap_or(true))
                    .map(|r| SessionSummary { id: r.id, title: r.title, workspace: r.workspace, updated: r.updated, messages: r.history.len() })
                    .collect()
            })
            .unwrap_or_default();
        out.sort_by(|a, b| b.updated.cmp(&a.updated).then(b.id.cmp(&a.id)));
        out
    }

    pub fn latest_for(&self, workspace: &Path) -> Option<SessionRecord> {
        self.list(Some(workspace)).first().and_then(|s| self.load(&s.id).ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_list_load_latest() {
        let d = tempfile::tempdir().unwrap();
        let st = SessionStore::at(d.path());
        let ws = PathBuf::from("/tmp/ws-a");
        for (i, t) in ["first task", "second task"].iter().enumerate() {
            let h = vec![Message::user(*t), Message::assistant("ok")];
            st.save(&SessionRecord {
                id: format!("2026010{}-000000-aaaa", i + 1),
                title: SessionStore::title_from(&h),
                workspace: ws.clone(),
                created: format!("2026-01-0{}T00:00:00Z", i + 1),
                updated: format!("2026-01-0{}T00:00:00Z", i + 1),
                provider: "mock".into(),
                history: h,
                total_usage: TokenUsage::default(),
            })
            .unwrap();
        }
        let l = st.list(Some(&ws));
        assert_eq!(l.len(), 2);
        assert_eq!(l[0].title, "second task");
        assert_eq!(st.latest_for(&ws).unwrap().history[0].content, "second task");
        assert!(st.list(Some(Path::new("/other"))).is_empty());
        assert!(st.load("../etc/passwd").is_err());
    }
}
