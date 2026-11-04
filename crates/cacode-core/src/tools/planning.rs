//! `todo_write` (agent-managed task list) and `exit_plan_mode`.

use super::ToolContext;
use crate::events::{AgentEvent, ApprovalDecision, ApprovalRequest, ApprovalTicket};
use crate::permission::PermissionMode;
use crate::types::TodoEntry;
use anyhow::{anyhow, Result};

pub fn todo_write(ctx: &ToolContext, args: &serde_json::Value) -> Result<String> {
    let arr = args.get("todos").and_then(|t| t.as_array()).ok_or_else(|| anyhow!("Missing todos array"))?;
    let mut list = Vec::new();
    for (i, t) in arr.iter().enumerate() {
        let content = t.get("content").or_else(|| t.get("text")).and_then(|c| c.as_str()).unwrap_or("").trim().to_string();
        if content.is_empty() {
            return Err(anyhow!("todo #{} has no content", i + 1));
        }
        let status = t.get("status").and_then(|s| s.as_str()).unwrap_or("pending").to_string();
        if !["pending", "in_progress", "completed"].contains(&status.as_str()) {
            return Err(anyhow!("todo #{} has invalid status '{}' (pending | in_progress | completed)", i + 1, status));
        }
        list.push(TodoEntry { content, status });
    }
    if let Ok(mut t) = ctx.todos.lock() {
        *t = list.clone();
    }
    if let Some(tx) = &ctx.events {
        let _ = tx.send(AgentEvent::Todos(list.clone()));
    }
    let done = list.iter().filter(|t| t.status == "completed").count();
    let body: Vec<String> = list
        .iter()
        .map(|t| format!("{} {}", match t.status.as_str() { "completed" => "[x]", "in_progress" => "[>]", _ => "[ ]" }, t.content))
        .collect();
    Ok(format!("Todo list updated ({}/{} completed):\n{}", done, list.len(), body.join("\n")))
}

pub async fn exit_plan_mode(ctx: &ToolContext, args: &serde_json::Value) -> Result<String> {
    if ctx.mode() != PermissionMode::Plan {
        return Err(anyhow!("exit_plan_mode is only valid in plan mode (current: {})", ctx.mode().label()));
    }
    let plan = args.get("plan").and_then(|p| p.as_str()).filter(|p| !p.trim().is_empty()).ok_or_else(|| anyhow!("Missing plan"))?;
    let Some(tx) = &ctx.events else {
        return Err(anyhow!("plan mode needs an interactive user to approve the plan; present the plan as your final answer instead"));
    };
    let (ticket, rx) = ApprovalTicket::new(ApprovalRequest {
        id: 0,
        tool: "exit_plan_mode".into(),
        summary: "Approve this plan and start implementing? (a = also auto-accept edits)".into(),
        diff: Some(plan.to_string()),
    });
    tx.send(AgentEvent::ApprovalRequired(ticket)).map_err(|_| anyhow!("front-end disconnected"))?;
    let decision = tokio::select! {
        d = rx => d.unwrap_or(ApprovalDecision::Reject(None)),
        _ = ctx.cancel.cancelled() => return Err(anyhow!("Cancelled by user")),
    };
    let new_mode = match decision {
        ApprovalDecision::Approve => PermissionMode::Ask,
        ApprovalDecision::ApproveAlways => PermissionMode::AutoEdit,
        ApprovalDecision::Reject(r) => {
            return Ok(format!("The user did NOT approve the plan{}. Stay in plan mode: revise the plan (ask clarifying questions if needed) and call exit_plan_mode again.", r.map(|r| format!(" — feedback: {}", r)).unwrap_or_default()))
        }
    };
    let mut c = ctx.clone();
    c.set_mode(new_mode);
    let _ = tx.send(AgentEvent::ModeChanged(new_mode));
    Ok(format!("Plan approved. Permission mode is now '{}'. Implement the plan step by step, keeping the todo list updated.", new_mode.label()))
}
