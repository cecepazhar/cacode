//! Core conversation types shared by the agent, LLM adapters and front-ends.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TokenUsage {
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
    /// True when the provider did not report usage and numbers are estimates.
    #[serde(default)]
    pub estimated: bool,
    /// Prompt tokens served from the provider's prompt cache (included in prompt_tokens).
    #[serde(default)]
    pub cache_read_tokens: i64,
    /// Prompt tokens written to the prompt cache (included in prompt_tokens).
    #[serde(default)]
    pub cache_write_tokens: i64,
}

impl TokenUsage {
    pub fn add(&mut self, other: &TokenUsage) {
        self.prompt_tokens += other.prompt_tokens;
        self.completion_tokens += other.completion_tokens;
        self.total_tokens += other.total_tokens;
        self.estimated |= other.estimated;
        self.cache_read_tokens += other.cache_read_tokens;
        self.cache_write_tokens += other.cache_write_tokens;
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ToolCall {
    /// Provider tool-call id (`call_…` / `toolu_…`). Synthesised for text-mode calls.
    #[serde(default)]
    pub id: String,
    pub tool: String,
    #[serde(default)]
    pub args: serde_json::Value,
}

/// Canonical conversation message.
///
/// * `role = "assistant"` may carry `tool_calls`.
/// * `role = "tool"` carries one tool result, linked by `tool_call_id`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
pub struct Message {
    pub role: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_error: bool,
}

impl Message {
    pub fn new(role: &str, content: impl Into<String>) -> Self {
        Self { role: role.into(), content: content.into(), ..Default::default() }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self::new("user", content)
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self::new("assistant", content)
    }
    pub fn system(content: impl Into<String>) -> Self {
        Self::new("system", content)
    }
    pub fn tool_result(call: &ToolCall, content: impl Into<String>, is_error: bool) -> Self {
        Self {
            role: "tool".into(),
            content: content.into(),
            tool_call_id: Some(call.id.clone()),
            name: Some(call.tool.clone()),
            is_error,
            ..Default::default()
        }
    }
}

/// Agent-managed task list entry (`todo_write` tool).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct TodoEntry {
    pub content: String,
    /// pending | in_progress | completed
    pub status: String,
}
