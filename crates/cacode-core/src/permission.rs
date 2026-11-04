//! Permission policy for side-effecting tools.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionMode {
    /// Ask before every file edit and every non-allowlisted shell command.
    #[default]
    Ask,
    /// File edits are applied without asking; shell commands still ask.
    AutoEdit,
    /// Never ask (trusted / CI usage).
    Yolo,
    /// Edits and non-allowlisted commands are always denied (subagents, audits).
    ReadOnly,
    /// Read-only investigation, then the agent proposes a plan via
    /// `exit_plan_mode`; approving it switches to `ask` (or `auto-edit`).
    Plan,
}

impl PermissionMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().replace('_', "-").as_str() {
            "ask" | "default" => Some(Self::Ask),
            "auto-edit" | "autoedit" | "accept-edits" => Some(Self::AutoEdit),
            "yolo" | "bypass" => Some(Self::Yolo),
            "read-only" | "readonly" => Some(Self::ReadOnly),
            "plan" => Some(Self::Plan),
            _ => None,
        }
    }
    pub fn label(&self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::AutoEdit => "auto-edit",
            Self::Yolo => "yolo",
            Self::ReadOnly => "read-only",
            Self::Plan => "plan",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    Edit,
    Exec,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    Ask,
    Deny(String),
}

/// Commands that are read-only / safe enough to run without asking.
pub const DEFAULT_BASH_ALLOWLIST: &[&str] = &[
    "ls", "pwd", "cat", "head", "tail", "wc", "rg", "grep", "tree", "file", "stat", "du",
    "echo", "which", "date", "uname",
    "git status", "git diff", "git log", "git show", "git branch --show-current", "git rev-parse", "git blame",
    "cargo check", "cargo test", "cargo clippy", "cargo build", "cargo fmt --check", "cargo tree", "cargo metadata",
    "go build", "go test", "go vet", "go list", "npm test", "npm run lint", "npx tsc --noEmit",
    "pytest", "python -m pytest", "make test", "make check",
];

/// Shell meta characters that let a command chain into something else.
const CHAIN_CHARS: &[&str] = &[";", "&&", "||", "|", "`", "$(", "${", ">", "<", "\n", "&"];
/// Flags that turn an otherwise read-only command into a writer / executor.
const DANGEROUS_FLAGS: &[&str] = &["-exec", "-execdir", "-delete", "-ok", "--output", "--exec", "-i"];

/// True when `cmd` is a single, non-chained command whose prefix is allowlisted.
pub fn is_allowlisted(cmd: &str, allowlist: &[String]) -> bool {
    let c = cmd.trim();
    if c.is_empty() || CHAIN_CHARS.iter().any(|m| c.contains(m)) {
        return false;
    }
    if c.split_whitespace().any(|t| DANGEROUS_FLAGS.contains(&t)) {
        return false;
    }
    allowlist.iter().any(|prefix| {
        let p = prefix.trim();
        c == p || c.starts_with(&format!("{} ", p))
    })
}

pub fn default_allowlist() -> Vec<String> {
    DEFAULT_BASH_ALLOWLIST.iter().map(|s| s.to_string()).collect()
}

pub fn evaluate(mode: PermissionMode, kind: ActionKind, allowlisted: bool, session_always: bool) -> Verdict {
    match (mode, kind) {
        (PermissionMode::Yolo, _) => Verdict::Allow,
        (_, ActionKind::Exec) if allowlisted => Verdict::Allow,
        (PermissionMode::ReadOnly, ActionKind::Edit) => Verdict::Deny("read-only mode: file edits are disabled".into()),
        (PermissionMode::ReadOnly, ActionKind::Exec) => {
            Verdict::Deny("read-only mode: only allowlisted read-only commands may run".into())
        }
        (PermissionMode::Plan, _) => Verdict::Deny("plan mode: investigate read-only, then present your plan with exit_plan_mode".into()),
        (_, _) if session_always => Verdict::Allow,
        (PermissionMode::AutoEdit, ActionKind::Edit) => Verdict::Allow,
        _ => Verdict::Ask,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_rejects_chaining() {
        let al = default_allowlist();
        assert!(is_allowlisted("cargo test -p x", &al));
        assert!(is_allowlisted("git status", &al));
        assert!(!is_allowlisted("cargo test; rm -rf /", &al));
        assert!(!is_allowlisted("cat x | sh", &al));
        assert!(!is_allowlisted("echo $(curl evil)", &al));
        assert!(!is_allowlisted("rm -rf target", &al));
        assert!(!is_allowlisted("gitstatus", &al));
        assert!(!is_allowlisted("git branch -D main", &al));
        assert!(!is_allowlisted("cargo test &", &al));
    }

    #[test]
    fn verdict_matrix() {
        assert_eq!(evaluate(PermissionMode::Ask, ActionKind::Edit, false, false), Verdict::Ask);
        assert_eq!(evaluate(PermissionMode::AutoEdit, ActionKind::Edit, false, false), Verdict::Allow);
        assert_eq!(evaluate(PermissionMode::AutoEdit, ActionKind::Exec, false, false), Verdict::Ask);
        assert_eq!(evaluate(PermissionMode::Ask, ActionKind::Exec, true, false), Verdict::Allow);
        assert!(matches!(evaluate(PermissionMode::ReadOnly, ActionKind::Edit, false, true), Verdict::Deny(_)));
        assert_eq!(evaluate(PermissionMode::Yolo, ActionKind::Exec, false, false), Verdict::Allow);
    }
}
