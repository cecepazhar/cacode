//! Parallel subagents for the `task` tool: fresh context, bounded
//! concurrency, timeouts, step / token budgets, and evidence-based reports.

use crate::commands::chat::{ReActAgent, TurnOutcome};
use crate::events::{AgentEvent, AgentEvents, SubagentStatus};
use crate::permission::PermissionMode;
use crate::tools::{SubagentSpawner, ToolContext};
use crate::types::Message;
use anyhow::{anyhow, Result};
use futures_util::future::BoxFuture;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::Semaphore;

pub const DEFAULT_SUBAGENT_STEPS: usize = 15;
pub const MAX_SUBAGENT_STEPS: usize = 40;
pub const DEFAULT_SUBAGENT_TIMEOUT: Duration = Duration::from_secs(300);
pub const DEFAULT_SUBAGENT_TOKEN_BUDGET: i64 = 200_000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Evidence {
    pub command: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub output_excerpt: String,
}

/// What the parent receives. `files_touched` and `evidence` come from the
/// tool layer's own logs, never from the subagent's prose.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SubagentReport {
    pub id: usize,
    pub description: String,
    pub outcome: String,
    pub summary: String,
    pub claims: Vec<String>,
    pub files_touched: Vec<String>,
    pub evidence: Vec<Evidence>,
    /// false when the summary claims success (tests pass / fixed / …) and
    /// no executed command backs it, or a failing command contradicts it.
    pub verified: bool,
    pub verification_note: String,
    pub steps_used: usize,
    pub tokens_used: i64,
}

fn success_claim_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"(?i)\b(tests?\s+(all\s+)?pass(ed|ing)?|all\s+tests?\s+pass|passes|0 failed|build succeeded|compiles?( cleanly| successfully)?|fixed|resolved|works now|verified|lulus|berhasil|sudah (aman|beres|selesai)|tidak ada error|no errors?)\b").expect("static regex")
    })
}

/// Pull `{"summary":..,"claims":[..]}` out of the final message if present.
fn parse_final(text: &str) -> (String, Vec<String>) {
    static FENCE: OnceLock<Regex> = OnceLock::new();
    let re = FENCE.get_or_init(|| Regex::new(r"(?s)```json\s*(\{.*?\})\s*```").expect("static regex"));
    if let Some(c) = re.captures_iter(text).last().and_then(|c| c.get(1)) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(c.as_str()) {
            let summary = v.get("summary").and_then(|s| s.as_str()).unwrap_or("").to_string();
            let claims = v.get("claims").and_then(|c| c.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
            if !summary.is_empty() {
                return (summary, claims);
            }
        }
    }
    (crate::text::clamp_output(text.trim(), 6_000, 120), Vec::new())
}

pub fn verify(summary: &str, claims: &[String], evidence: &[Evidence]) -> (bool, String) {
    let text = format!("{} {}", summary, claims.join(" "));
    let claims_success = success_claim_re().is_match(&text);
    if !claims_success {
        return (true, "no success claims to verify".into());
    }
    if evidence.is_empty() {
        return (false, "UNVERIFIED: claims success but ran no commands — treat as unproven".into());
    }
    let failing: Vec<&Evidence> = evidence.iter().filter(|e| e.exit_code != Some(0)).collect();
    let last = evidence.last().expect("non-empty");
    if last.exit_code != Some(0) {
        return (false, format!("CONTRADICTED: claims success but the last command `{}` exited {:?}", last.command, last.exit_code));
    }
    if failing.is_empty() {
        (true, format!("backed by {} successful command(s)", evidence.len()))
    } else {
        (true, format!("backed by final successful command `{}` ({} earlier command(s) failed)", last.command, failing.len()))
    }
}

pub struct SubagentRuntime {
    template: ReActAgent,
    semaphore: Arc<Semaphore>,
    events: Option<AgentEvents>,
    counter: AtomicUsize,
    pub timeout: Duration,
    pub token_budget: i64,
}

impl SubagentRuntime {
    pub fn new(parent: &ReActAgent, limit: usize) -> Self {
        let mut template = parent.clone();
        template.history.clear();
        template.tool_ctx = None;
        template.event_tx = None;
        template.is_subagent = true;
        template.total_usage = Default::default();
        template.last_usage = Default::default();
        template.total_calls = 0;
        Self {
            template,
            semaphore: Arc::new(Semaphore::new(limit.clamp(1, 49))),
            events: parent.event_tx.clone(),
            counter: AtomicUsize::new(0),
            timeout: DEFAULT_SUBAGENT_TIMEOUT,
            token_budget: DEFAULT_SUBAGENT_TOKEN_BUDGET,
        }
    }

    pub fn limit(&self) -> usize {
        self.semaphore.available_permits()
    }

    fn emit(&self, id: usize, description: &str, status: SubagentStatus) {
        if let Some(tx) = &self.events {
            let _ = tx.send(AgentEvent::Subagent { id, description: description.to_string(), status });
        }
    }

    pub async fn run(&self, args: serde_json::Value, parent: ToolContext) -> Result<SubagentReport> {
        let description = args.get("description").and_then(|v| v.as_str()).unwrap_or("subtask").to_string();
        let prompt = args.get("prompt").and_then(|v| v.as_str()).ok_or_else(|| anyhow!("Missing prompt"))?.to_string();
        let want_edit = args.get("mode").and_then(|v| v.as_str()) == Some("edit");
        let mode = if want_edit {
            match parent.mode() {
                PermissionMode::AutoEdit | PermissionMode::Yolo => parent.mode(),
                other => {
                    return Err(anyhow!(
                        "edit-mode subagents run unattended and need permission mode auto-edit or yolo (current: {}). Use mode=read_only and apply the edits yourself.",
                        other.label()
                    ))
                }
            }
        } else {
            PermissionMode::ReadOnly
        };
        let max_steps = args.get("max_steps").and_then(|v| v.as_u64()).map(|v| v as usize).unwrap_or(DEFAULT_SUBAGENT_STEPS).clamp(1, MAX_SUBAGENT_STEPS);

        let permit = tokio::select! {
            p = self.semaphore.clone().acquire_owned() => p.map_err(|_| anyhow!("subagent pool closed"))?,
            _ = parent.cancel.cancelled() => return Err(anyhow!("Cancelled before the subagent started")),
        };
        let id = self.counter.fetch_add(1, Ordering::SeqCst) + 1;
        self.emit(id, &description, SubagentStatus::Started);

        let child_cancel = parent.cancel.child_token();
        let child_ctx = parent.child(mode, child_cancel.clone());
        let mut agent = self.template.clone();
        agent.permission_mode = mode;
        agent.max_steps = max_steps;
        agent.turn_token_budget = self.token_budget;
        agent.workspace_root = Some(child_ctx.workspace.root().to_path_buf());
        agent.tool_ctx = Some(child_ctx.clone());
        agent.history = vec![Message::user(prompt)];

        let outcome = match tokio::time::timeout(self.timeout, agent.run_loop(None, child_cancel.clone())).await {
            Ok(o) => o,
            Err(_) => {
                child_cancel.cancel();
                TurnOutcome::Failed(format!("watchdog: exceeded {}s", self.timeout.as_secs()))
            }
        };
        drop(permit);

        let final_text = agent.history.iter().rev().find(|m| m.role == "assistant" && m.tool_calls.is_empty()).map(|m| m.content.clone()).unwrap_or_default();
        let (summary, claims) = parse_final(&final_text);
        let evidence: Vec<Evidence> = child_ctx
            .exec_log
            .lock()
            .map(|l| l.iter().map(|r| Evidence { command: r.command.clone(), exit_code: r.exit_code, timed_out: r.timed_out, output_excerpt: r.output_excerpt.clone() }).collect())
            .unwrap_or_default();
        let files: Vec<String> = child_ctx.files_touched.lock().map(|s| s.iter().cloned().collect()).unwrap_or_default();
        if let Ok(mut pf) = parent.files_touched.lock() {
            pf.extend(files.iter().cloned());
        }
        let (mut verified, mut note) = verify(&summary, &claims, &evidence);
        let outcome_label = match &outcome {
            TurnOutcome::Finished => "finished".to_string(),
            TurnOutcome::StepBudget => format!("stopped: step budget ({}) exhausted", max_steps),
            TurnOutcome::LoopDetected => "stopped: loop detected".into(),
            TurnOutcome::TokenBudget => format!("stopped: token budget ({}) exhausted", self.token_budget),
            TurnOutcome::Cancelled => "cancelled".into(),
            TurnOutcome::Failed(e) => format!("failed: {}", e),
        };
        if outcome != TurnOutcome::Finished && summary.is_empty() {
            verified = false;
            note = format!("no final report ({})", outcome_label);
        }
        let report = SubagentReport {
            id,
            description: description.clone(),
            outcome: outcome_label.clone(),
            summary,
            claims,
            files_touched: files,
            evidence,
            verified,
            verification_note: note,
            steps_used: agent.history.iter().filter(|m| m.role == "assistant").count(),
            tokens_used: agent.total_usage.total_tokens,
        };
        self.emit(
            id,
            &description,
            if matches!(outcome, TurnOutcome::Failed(_) | TurnOutcome::Cancelled) { SubagentStatus::Failed(outcome_label) } else { SubagentStatus::Finished { verified: report.verified } },
        );
        Ok(report)
    }
}

impl SubagentSpawner for SubagentRuntime {
    fn spawn(&self, args: serde_json::Value, parent: ToolContext) -> BoxFuture<'static, Result<String>> {
        // The runtime lives in an Arc inside ToolContext; clone what the future needs.
        let me = SubagentRuntime {
            template: self.template.clone(),
            semaphore: self.semaphore.clone(),
            events: self.events.clone(),
            counter: AtomicUsize::new(0),
            timeout: self.timeout,
            token_budget: self.token_budget,
        };
        let base_id = self.counter.fetch_add(1, Ordering::SeqCst);
        me.counter.store(base_id, Ordering::SeqCst);
        Box::pin(async move {
            let r = me.run(args, parent).await?;
            let header = if r.verified { "SUBAGENT REPORT" } else { "SUBAGENT REPORT — ⚠ UNVERIFIED" };
            Ok(format!("{}\n```json\n{}\n```", header, serde_json::to_string_pretty(&r)?))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(cmd: &str, code: i32) -> Evidence {
        Evidence { command: cmd.into(), exit_code: Some(code), timed_out: false, output_excerpt: String::new() }
    }

    #[test]
    fn claims_without_evidence_are_unverified() {
        let (ok, note) = verify("All tests pass and the bug is fixed", &[], &[]);
        assert!(!ok);
        assert!(note.starts_with("UNVERIFIED"));
        let (ok2, _) = verify("Semua test lulus", &[], &[ev("cargo test", 0)]);
        assert!(ok2);
        let (ok3, note3) = verify("tests pass", &[], &[ev("cargo test", 101)]);
        assert!(!ok3);
        assert!(note3.starts_with("CONTRADICTED"));
        let (ok4, _) = verify("Found 3 call sites of parse_tool_calls in chat.rs", &[], &[]);
        assert!(ok4);
    }

    #[test]
    fn parse_final_json() {
        let t = "done\n```json\n{\"summary\": \"found it\", \"claims\": [\"a\", \"b\"]}\n```";
        let (s, c) = parse_final(t);
        assert_eq!(s, "found it");
        assert_eq!(c, vec!["a", "b"]);
    }
}
