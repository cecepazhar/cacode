use anyhow::{anyhow, Result};
use reedline::{DefaultPrompt, Reedline, Signal};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::process::Command;
use std::time::Instant;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use walkdir::WalkDir;
use regex::Regex;

pub const CA_VERSION: &str = "1.0.5";

pub type AgentEvents = mpsc::UnboundedSender<AgentEvent>;

/// Events streamed from the background ReAct worker back to the TUI,
/// so the event loop never blocks on LLM calls or tool execution.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    Thinking(String),
    AssistantChunk(String),
    ToolStart { tool: String, args: String },
    ToolOutput { tool: String, output: String, is_error: bool },
    TurnFinished {
        usage: TokenUsage,
        total_calls: usize,
        duration_ms: u64,
        history: Vec<Message>,
        total_usage: TokenUsage,
        last_usage: TokenUsage,
    },
    Error(String),
    Cancelled,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct OpenAIChoice {
    message: Message,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct OpenAIResponse {
    choices: Vec<OpenAIChoice>,
}

#[derive(Debug, Deserialize)]
struct AnthropicUsage {
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct AnthropicResponse {
    content: Vec<AnthropicContent>,
    usage: Option<AnthropicUsage>,
}

#[derive(Debug, Deserialize)]
struct AnthropicContent {
    #[serde(rename = "type")]
    #[allow(dead_code)]
    typ: String,
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OllamaResponse {
    message: Message,
    prompt_eval_count: Option<i64>,
    eval_count: Option<i64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToolCall {
    pub tool: String,
    pub args: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Provider {
    OpenAI { model: String },
    Anthropic { model: String },
    Ollama { model: String },
    Router9 { model: String },
    AntigravityZen { model: String },
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TokenUsage {
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
}

#[derive(Clone)]
pub struct ReActAgent {
    client: Client,
    pub history: Vec<Message>,
    pub provider: Provider,
    pub user_name: String,
    pub persona: String,
    pub thinking_mode: String,
    pub context_window_limit: u64,
    pub last_usage: TokenUsage,
    pub total_usage: TokenUsage,
    pub total_calls: usize,
}

impl Default for ReActAgent {
    fn default() -> Self {
        Self::new()
    }
}

impl ReActAgent {
    pub fn detect_provider() -> Provider {
        if env::var("ANTHROPIC_API_KEY").is_ok() {
            let model = env::var("ANTHROPIC_MODEL").unwrap_or_else(|_| "claude-3-5-sonnet-20240620".into());
            Provider::Anthropic { model }
        } else if env::var("OPENAI_API_KEY").is_ok() {
            let model = env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-4o".into());
            Provider::OpenAI { model }
        } else if env::var("ANTIGRAVITY_API_KEY").is_ok() || env::var("ANTIGRAVITY_ZEN_API_KEY").is_ok() {
            let model = env::var("ANTIGRAVITY_ZEN_MODEL")
                .or_else(|_| env::var("ANTIGRAVITY_MODEL"))
                .unwrap_or_else(|_| "deepseek-v4-flash-free".into());
            Provider::AntigravityZen { model }
        } else if env::var("OLLAMA_MODEL").is_ok() || env::var("OLLAMA_URL").is_ok() {
            let model = env::var("OLLAMA_MODEL").unwrap_or_else(|_| "llama3".into());
            Provider::Ollama { model }
        } else {
            let model = env::var("ROUTER9_MODEL")
                .map(|m| if m == "default" || m.is_empty() { "ZA126_PRO".to_string() } else { m })
                .unwrap_or_else(|_| "ZA126_PRO".into());
            Provider::Router9 { model }
        }
    }

    pub fn new() -> Self {
        Self {
            client: Client::new(),
            history: Vec::new(),
            provider: Self::detect_provider(),
            user_name: "Prof. Cecep".into(),
            persona: "You are CACode, an elite sovereign Rust development assistant. The user is Prof. Cecep (Cecep Saeful Azhar Hidayat, ST). Always greet and address him respectfully as 'Prof. Cecep' (e.g. 'Hi Prof. Cecep', 'Siap, Prof. Cecep.'). Be direct, concise, and technically precise.".into(),
            thinking_mode: "Fast".into(),
            context_window_limit: 200_000,
            last_usage: TokenUsage::default(),
            total_usage: TokenUsage::default(),
            total_calls: 0,
        }
    }

    pub fn add_user(&mut self, content: &str) {
        self.history.push(Message { role: "user".into(), content: content.into() });
    }

    pub fn add_assistant(&mut self, content: &str) {
        self.history.push(Message { role: "assistant".into(), content: content.into() });
    }

    pub fn system_prompt(&self) -> Message {
        let persona_intro = if !self.persona.is_empty() {
            self.persona.clone()
        } else {
            format!(
                "You are CACode, a sovereign Rust-focused development agent. The user is {}. Address the user respectfully as {} when appropriate.",
                self.user_name, self.user_name
            )
        };

        let thinking_hint = match self.thinking_mode.as_str() {
            "Analytical" => "\n\n[Cognitive Mode: Analytical — analyze problems with structured step-by-step logic before answering or using tools.]",
            "Deep" => "\n\n[Cognitive Mode: Deep Reasoning — apply comprehensive adversarial verification, edge-case proofs, and rigorous architectural checks.]",
            _ => "",
        };

        let content = format!(
            "{}{}\n\nAvailable tools:\n\
            1. read_file: {{\"tool\": \"read_file\", \"args\": {{\"path\": \"...\", \"offset\": 1, \"limit\": 200}}}}\n\
            2. patch_file: {{\"tool\": \"patch_file\", \"args\": {{\"path\": \"...\", \"old_string\": \"...\", \"new_string\": \"...\"}}}}\n\
            3. search_replace: {{\"tool\": \"search_replace\", \"args\": {{\"path\": \"...\", \"search\": \"...\", \"replace\": \"...\"}}}}\n\
            4. write_file: {{\"tool\": \"write_file\", \"args\": {{\"path\": \"...\", \"content\": \"...\"}}}}\n\
            5. bash: {{\"tool\": \"bash\", \"args\": {{\"command\": \"...\"}}}}\n\
            6. search_files: {{\"tool\": \"search_files\", \"args\": {{\"pattern\": \"...\", \"dir\": \".\"}}}}\n\n\
            When you need to act, output a JSON array of tool calls strictly wrapped in ```json ... ``` codeblock:\n\
            ```json\n[\n  {{\"tool\": \"read_file\", \"args\": {{\"path\": \"src/main.rs\"}}}}\n]\n```\n\
            You can also provide edits using standard Aider search/replace blocks:\n\
            path/to/file.rs\n\
            <<<<<<< SEARCH\n\
            [old code]\n\
            =======\n\
            [new code]\n\
            >>>>>>> REPLACE\n\n\
            If no tool action is needed, reply normally with natural language.",
            persona_intro, thinking_hint
        );

        Message {
            role: "system".into(),
            content,
        }
    }

    pub fn record_usage_to_sqlite(model: &str, usage: &TokenUsage) {
        if usage.total_tokens == 0 {
            return;
        }
        if let Some(home) = dirs::home_dir() {
            let db_path = home.join(".cacode").join("usage.db");
            if let Some(parent) = db_path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            if let Ok(conn) = rusqlite::Connection::open(&db_path) {
                let _ = conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=3000;");
                let _ = conn.execute(
                    "CREATE TABLE IF NOT EXISTS token_usage (
                        id INTEGER PRIMARY KEY AUTOINCREMENT,
                        timestamp TEXT NOT NULL,
                        model TEXT NOT NULL,
                        prompt_tokens INTEGER NOT NULL,
                        completion_tokens INTEGER NOT NULL,
                        total_tokens INTEGER NOT NULL,
                        est_cost REAL NOT NULL
                    )",
                    [],
                );
                let est_cost = (usage.prompt_tokens as f64 * 2.50 + usage.completion_tokens as f64 * 10.0) / 1_000_000.0;
                let ts = chrono::Utc::now().to_rfc3339();
                let _ = conn.execute(
                    "INSERT INTO token_usage (timestamp, model, prompt_tokens, completion_tokens, total_tokens, est_cost) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    rusqlite::params![ts, model, usage.prompt_tokens, usage.completion_tokens, usage.total_tokens, est_cost],
                );
            }
        }
    }

    pub fn extract_response_content(text: &str) -> Result<(String, TokenUsage)> {
        let trimmed = text.trim();
        let mut usage = TokenUsage::default();

        // 1. Try parsing standard JSON
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(trimmed) {
            if let Some(err) = parsed.get("error").and_then(|e| e.get("message")).and_then(|m| m.as_str()) {
                return Err(anyhow!("API error: {}", err));
            }
            if let Some(u) = parsed.get("usage") {
                let p = u.get("prompt_tokens").or_else(|| u.get("input_tokens")).and_then(|v| v.as_i64()).unwrap_or(0);
                let c = u.get("completion_tokens").or_else(|| u.get("output_tokens")).and_then(|v| v.as_i64()).unwrap_or(0);
                let t = u.get("total_tokens").and_then(|v| v.as_i64()).unwrap_or(p + c);
                usage.prompt_tokens = p;
                usage.completion_tokens = c;
                usage.total_tokens = t;
            }
            if let Some(content) = parsed.get("choices")
                .and_then(|c| c.as_array())
                .and_then(|a| a.first())
                .and_then(|c| c.get("message"))
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_str())
            {
                if usage.total_tokens == 0 {
                    usage.completion_tokens = (content.len() / 4).max(1) as i64;
                    usage.prompt_tokens = 50;
                    usage.total_tokens = usage.prompt_tokens + usage.completion_tokens;
                }
                return Ok((content.to_string(), usage));
            }
        }

        // 2. Fallback: Parse Server-Sent Events (SSE chunks starting with "data:")
        if trimmed.starts_with("data:") || trimmed.contains("\ndata:") {
            let mut accumulated = String::new();
            for line in trimmed.lines() {
                let line = line.trim();
                if let Some(data) = line.strip_prefix("data:") {
                    let data = data.trim();
                    if data == "[DONE]" || data.is_empty() {
                        continue;
                    }
                    if let Ok(chunk) = serde_json::from_str::<serde_json::Value>(data) {
                        if let Some(err) = chunk.get("error").and_then(|e| e.get("message")).and_then(|m| m.as_str()) {
                            return Err(anyhow!("Stream error: {}", err));
                        }
                        if let Some(u) = chunk.get("usage") {
                            let p = u.get("prompt_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
                            let c = u.get("completion_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
                            let t = u.get("total_tokens").and_then(|v| v.as_i64()).unwrap_or(p + c);
                            if t > 0 {
                                usage.prompt_tokens = p;
                                usage.completion_tokens = c;
                                usage.total_tokens = t;
                            }
                        }
                        if let Some(choices) = chunk.get("choices").and_then(|c| c.as_array()) {
                            if let Some(first) = choices.first() {
                                if let Some(content) = first.get("delta").and_then(|d| d.get("content")).and_then(|c| c.as_str()) {
                                    accumulated.push_str(content);
                                } else if let Some(content) = first.get("message").and_then(|m| m.get("content")).and_then(|c| c.as_str()) {
                                    accumulated.push_str(content);
                                }
                            }
                        }
                    }
                }
            }
            if !accumulated.is_empty() {
                if usage.total_tokens == 0 {
                    usage.completion_tokens = (accumulated.len() / 4).max(1) as i64;
                    usage.prompt_tokens = 50;
                    usage.total_tokens = usage.prompt_tokens + usage.completion_tokens;
                }
                return Ok((accumulated, usage));
            }
        }

        Err(anyhow!(
            "Invalid response from model: {}",
            text.chars().take(200).collect::<String>()
        ))
    }

    pub async fn compress_history(&mut self) -> Result<String> {
        if self.history.len() <= 2 {
            return Ok("Context memory is already compact (less than 3 messages).".into());
        }
        let total_chars_before: usize = self.history.iter().map(|m| m.content.len()).sum();
        let old_count = self.history.len();

        let mut transcript = String::new();
        for m in &self.history {
            transcript.push_str(&format!("[{}]: {}\n", m.role, m.content));
        }

        let prompt = format!(
            "Summarize this conversation context into a dense, high-signal technical briefing. \
            Extract all essential requirements, decisions, active file paths, and current progress. \
            Begin with '[COMPACTED CONTEXT SUMMARY]'.\n\n{}",
            transcript
        );

        let summary = self.call_llm(&prompt).await?;
        let last_user = self.history.iter().rev().find(|m| m.role == "user").cloned();

        self.history.clear();
        self.history.push(Message {
            role: "assistant".into(),
            content: summary,
        });
        if let Some(u) = last_user {
            self.history.push(u);
        }

        let total_chars_after: usize = self.history.iter().map(|m| m.content.len()).sum();
        let pct = (total_chars_before.saturating_sub(total_chars_after) * 100)
            .checked_div(total_chars_before)
            .unwrap_or(0);

        Ok(format!(
            "Compressed {} messages into compact summary (~{}% character reduction). Context window optimized.",
            old_count, pct
        ))
    }

    pub async fn call_llm(&mut self, user_input: &str) -> Result<String> {
        self.total_calls += 1;
        let mut msgs = vec![self.system_prompt()];
        msgs.extend(self.history.clone());
        msgs.push(Message { role: "user".into(), content: user_input.into() });

        let (content, usage) = match &self.provider {
            Provider::OpenAI { model } => {
                let api_key = env::var("OPENAI_API_KEY")?;
                let base_url = env::var("OPENAI_BASE_URL").unwrap_or_else(|_| "https://api.openai.com".into());
                let base = base_url.trim_end_matches('/');
                let endpoint = if base.ends_with("/v1") {
                    format!("{}/chat/completions", base)
                } else {
                    format!("{}/v1/chat/completions", base)
                };
                let resp = self.client
                    .post(&endpoint)
                    .bearer_auth(api_key)
                    .json(&serde_json::json!({
                        "model": model,
                        "messages": msgs,
                        "stream": false
                    }))
                    .send()
                    .await?;
                let status = resp.status();
                let bytes = resp.bytes().await?;
                let text = String::from_utf8_lossy(&bytes).to_string();
                if !status.is_success() {
                    return Err(anyhow!(
                        "OpenAI HTTP {} (endpoint: {}) — {}",
                        status,
                        endpoint,
                        text.chars().take(500).collect::<String>()
                    ));
                }
                Self::extract_response_content(&text)?
            }
            Provider::Anthropic { model } => {
                let api_key = env::var("ANTHROPIC_API_KEY")?;
                let non_system_msgs: Vec<_> = msgs.iter()
                    .filter(|m| m.role != "system")
                    .map(|m| serde_json::json!({
                        "role": m.role,
                        "content": m.content
                    }))
                    .collect();
                let resp = self.client
                    .post("https://api.anthropic.com/v1/messages")
                    .header("x-api-key", api_key)
                    .header("anthropic-version", "2023-06-01")
                    .json(&serde_json::json!({
                        "model": model,
                        "max_tokens": 4096,
                        "system": self.system_prompt().content,
                        "messages": non_system_msgs,
                    }))
                    .send()
                    .await?
                    .json::<AnthropicResponse>()
                    .await?;
                let txt = resp.content.into_iter().filter_map(|c| c.text).collect::<Vec<_>>().join("\n");
                let usage = if let Some(u) = resp.usage {
                    let prompt = u.input_tokens.unwrap_or(0);
                    let comp = u.output_tokens.unwrap_or(0);
                    TokenUsage {
                        prompt_tokens: prompt,
                        completion_tokens: comp,
                        total_tokens: prompt + comp,
                    }
                } else {
                    let comp = (txt.len() / 4).max(1) as i64;
                    TokenUsage {
                        prompt_tokens: 50,
                        completion_tokens: comp,
                        total_tokens: 50 + comp,
                    }
                };
                (txt, usage)
            }
            Provider::Ollama { model } => {
                let base_url = env::var("OLLAMA_URL").unwrap_or_else(|_| "http://127.0.0.1:11434".into());
                let endpoint = format!("{}/api/chat", base_url.trim_end_matches('/'));
                let resp = self.client
                    .post(&endpoint)
                    .json(&serde_json::json!({
                        "model": model,
                        "messages": msgs,
                        "stream": false
                    }))
                    .send()
                    .await?
                    .json::<OllamaResponse>()
                    .await?;
                let txt = resp.message.content;
                let usage = if let (Some(p), Some(c)) = (resp.prompt_eval_count, resp.eval_count) {
                    TokenUsage {
                        prompt_tokens: p,
                        completion_tokens: c,
                        total_tokens: p + c,
                    }
                } else {
                    let comp = (txt.len() / 4).max(1) as i64;
                    TokenUsage {
                        prompt_tokens: 50,
                        completion_tokens: comp,
                        total_tokens: 50 + comp,
                    }
                };
                (txt, usage)
            }
            Provider::Router9 { model } => {
                let base_url = env::var("ROUTER9_URL").unwrap_or_else(|_| "http://localhost:20128".into());
                let api_key = env::var("ROUTER9_API_KEY").unwrap_or_else(|_| "dummy".into());
                let base = base_url.trim_end_matches('/');
                let endpoint = if base.ends_with("/v1") || base.ends_with("/api") {
                    format!("{}/chat/completions", base)
                } else {
                    format!("{}/v1/chat/completions", base)
                };
                let resp = self.client
                    .post(&endpoint)
                    .bearer_auth(api_key)
                    .json(&serde_json::json!({
                        "model": model,
                        "messages": msgs,
                        "stream": false
                    }))
                    .send()
                    .await?;
                let status = resp.status();
                let bytes = resp.bytes().await?;
                let text = String::from_utf8_lossy(&bytes).to_string();
                if !status.is_success() {
                    return Err(anyhow!(
                        "9router HTTP {} (endpoint: {}) — {}",
                        status,
                        endpoint,
                        text.chars().take(500).collect::<String>()
                    ));
                }
                Self::extract_response_content(&text)?
            }
            Provider::AntigravityZen { model } => {
                let api_key = env::var("ANTIGRAVITY_API_KEY")
                    .or_else(|_| env::var("ANTIGRAVITY_ZEN_API_KEY"))
                    .unwrap_or_else(|_| "dummy".into());
                let endpoint = "https://antigravity.ai/zen/v1/chat/completions";
                let resp = self.client
                    .post(endpoint)
                    .bearer_auth(api_key)
                    .json(&serde_json::json!({
                        "model": model,
                        "messages": msgs,
                        "stream": false
                    }))
                    .send()
                    .await?;
                let status = resp.status();
                let bytes = resp.bytes().await?;
                let text = String::from_utf8_lossy(&bytes).to_string();
                if !status.is_success() {
                    return Err(anyhow!(
                        "Antigravity Zen HTTP {} (endpoint: {}) — {}",
                        status,
                        endpoint,
                        text.chars().take(500).collect::<String>()
                    ));
                }
                Self::extract_response_content(&text)?
            }
        };

        // Record metrics
        self.last_usage = usage.clone();
        self.total_usage.prompt_tokens += usage.prompt_tokens;
        self.total_usage.completion_tokens += usage.completion_tokens;
        self.total_usage.total_tokens += usage.total_tokens;

        let model_name = match &self.provider {
            Provider::OpenAI { model } | Provider::Anthropic { model }
            | Provider::Ollama { model } | Provider::Router9 { model }
            | Provider::AntigravityZen { model } => model.clone(),
        };
        Self::record_usage_to_sqlite(&model_name, &usage);

        Ok(content)
    }

    pub fn parse_tool_calls(&self, content: &str) -> Vec<ToolCall> {
        let json_block_re = Regex::new(r"(?s)```json\s*(\[.*?\])\s*```").unwrap();
        if let Some(caps) = json_block_re.captures(content) {
            if let Some(mat) = caps.get(1) {
                if let Ok(calls) = serde_json::from_str::<Vec<ToolCall>>(mat.as_str()) {
                    return calls;
                }
            }
        }
        if let Ok(calls) = serde_json::from_str::<Vec<ToolCall>>(content.trim()) {
            return calls;
        }

        // Auto-detection of Aider-style Search/Replace blocks:
        // If LLM outputs <<<<<<< SEARCH blocks with file paths without strict JSON,
        // parse and convert them to search_replace ToolCalls.
        let sr_blocks = crate::search_replace::parse_search_replace_blocks(content);
        if !sr_blocks.is_empty() {
            let mut calls = Vec::new();
            for block in sr_blocks {
                if let Some(path) = block.file_path {
                    calls.push(ToolCall {
                        tool: "search_replace".to_string(),
                        args: serde_json::json!({
                            "path": path,
                            "search": block.search,
                            "replace": block.replace,
                        }),
                    });
                }
            }
            if !calls.is_empty() {
                return calls;
            }
        }

        Vec::new()
    }

    pub fn exec_tool(&self, call: &ToolCall) -> Result<String> {
        match call.tool.as_str() {
            "read_file" => {
                let path = call.args.get("path").and_then(|v| v.as_str()).ok_or_else(|| anyhow!("Missing path"))?;
                let offset = call.args.get("offset").and_then(|v| v.as_u64()).unwrap_or(1) as usize;
                let limit = call.args.get("limit").and_then(|v| v.as_u64()).unwrap_or(200) as usize;
                let text = fs::read_to_string(path)?;
                let lines: Vec<&str> = text.lines().skip(offset.saturating_sub(1)).take(limit).collect();
                Ok(lines.join("\n"))
            }
            "write_file" => {
                let path = call.args.get("path").and_then(|v| v.as_str()).ok_or_else(|| anyhow!("Missing path"))?;
                let content = call.args.get("content").and_then(|v| v.as_str()).ok_or_else(|| anyhow!("Missing content"))?;
                if let Some(parent) = std::path::Path::new(path).parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(path, content)?;
                Ok(format!("Successfully wrote {} bytes to {}", content.len(), path))
            }
            "patch_file" => {
                let path = call.args.get("path").and_then(|v| v.as_str()).ok_or_else(|| anyhow!("Missing path"))?;
                let old_str = call.args.get("old_string").and_then(|v| v.as_str()).ok_or_else(|| anyhow!("Missing old_string"))?;
                let new_str = call.args.get("new_string").and_then(|v| v.as_str()).ok_or_else(|| anyhow!("Missing new_string"))?;
                let content = fs::read_to_string(path)?;
                let patched = crate::search_replace::apply_search_replace(&content, old_str, new_str)?;
                fs::write(path, patched)?;
                Ok(format!("Successfully patched {}", path))
            }
            "search_replace" => {
                let path = call.args.get("path").and_then(|v| v.as_str()).ok_or_else(|| anyhow!("Missing path"))?;
                let search = call.args.get("search").and_then(|v| v.as_str()).ok_or_else(|| anyhow!("Missing search"))?;
                let replace = call.args.get("replace").and_then(|v| v.as_str()).ok_or_else(|| anyhow!("Missing replace"))?;
                let content = fs::read_to_string(path)?;
                let patched = crate::search_replace::apply_search_replace(&content, search, replace)?;
                fs::write(path, patched)?;
                Ok(format!("Successfully applied search/replace to {}", path))
            }
            "bash" => {
                let cmd = call.args.get("command").and_then(|v| v.as_str()).ok_or_else(|| anyhow!("Missing command"))?;
                let output = Command::new("sh").arg("-c").arg(cmd).output()?;
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                Ok(format!("Exit: {}\nStdout:\n{}\nStderr:\n{}", output.status, stdout, stderr))
            }
            "search_files" => {
                let pattern = call.args.get("pattern").and_then(|v| v.as_str()).ok_or_else(|| anyhow!("Missing pattern"))?;
                let dir = call.args.get("dir").and_then(|v| v.as_str()).unwrap_or(".");
                let re = Regex::new(pattern)?;
                let mut matches = Vec::new();
                for entry in WalkDir::new(dir).into_iter().filter_map(|e| e.ok()) {
                    if entry.file_type().is_file() {
                        let path = entry.path();
                        if let Ok(content) = fs::read_to_string(path) {
                            if re.is_match(&content) {
                                matches.push(path.display().to_string());
                            }
                        }
                    }
                }
                Ok(matches.join("\n"))
            }
            other => Err(anyhow!("Unknown tool: {}", other)),
        }
    }

    pub async fn step(&mut self, input: &str) -> Result<()> {
        if input.trim().is_empty() {
            return Ok(());
        }

        if input.starts_with('/') {
            match input.trim() {
                "/help" => {
                    println!("Slash commands:");
                    println!("  /help     - Show this help summary");
                    println!("  /clear    - Clear conversation memory");
                    println!("  /exit     - Exit REPL session");
                    return Ok(());
                }
                "/clear" => {
                    self.history.clear();
                    println!("Session context cleared.");
                    return Ok(());
                }
                "/exit" => {
                    println!("Goodbye.");
                    std::process::exit(0);
                }
                cmd => {
                    println!("Unknown command: {}", cmd);
                    return Ok(());
                }
            }
        }

        self.add_user(input);

        let mut current_prompt = input.to_string();
        for _ in 0..5 {
            println!("Thinking...");
            let response = self.call_llm(&current_prompt).await?;
            let tool_calls = self.parse_tool_calls(&response);

            if tool_calls.is_empty() {
                println!("\n{}\n", response);
                self.add_assistant(&response);
                break;
            } else {
                println!("\n{}\n", response);
                self.add_assistant(&response);

                let mut tool_results = Vec::new();
                for call in tool_calls {
                    println!("[Tool: {}] Args: {}", call.tool, call.args);
                    match self.exec_tool(&call) {
                        Ok(res) => {
                            println!("[Tool Result]:\n{}", res);
                            tool_results.push(format!("Tool '{}' output:\n{}", call.tool, res));
                        }
                        Err(e) => {
                            println!("[Tool Error]: {}", e);
                            tool_results.push(format!("Tool '{}' error: {}", call.tool, e));
                        }
                    }
                }
                current_prompt = format!("Observation/Tool Results:\n{}", tool_results.join("\n\n"));
            }
        }

        Ok(())
    }

    /// Non-blocking ReAct turn worker. Intended to be spawned on the Tokio
    /// runtime so that the synchronous TUI event loop can keep rendering and
    /// ingesting keys. Streams events through `events`, accepts mid-turn
    /// steering instructions through `steering_rx`, and is aborted
    /// immediately by `cancel`.
    pub async fn run_turn(
        &mut self,
        prompt: String,
        events: AgentEvents,
        mut steering_rx: mpsc::UnboundedReceiver<String>,
        cancel: CancellationToken,
    ) {
        let started = Instant::now();
        self.add_user(&prompt);
        let mut current_prompt = prompt;

        for step in 0..5 {
            if cancel.is_cancelled() {
                let _ = events.send(AgentEvent::Cancelled);
                return;
            }

            // Mid-run steering: fold any pending instruction into this step.
            while let Ok(steer) = steering_rx.try_recv() {
                self.add_user(&format!("[Steering / Mid-turn] {}", steer));
                current_prompt = format!(
                    "{}\n\n[Steering / Mid-turn] Incorporate this user instruction into your next reasoning step:\n{}",
                    current_prompt, steer
                );
            }

            let _ = events.send(AgentEvent::Thinking(format!("ReAct step {} / 5", step + 1)));

            let call = async {
                self.call_llm(&current_prompt).await
            };
            let response = tokio::select! {
                _ = cancel.cancelled() => {
                    let _ = events.send(AgentEvent::Cancelled);
                    return;
                }
                res = call => res,
            };

            match response {
                Ok(resp) => {
                    let tool_calls = self.parse_tool_calls(&resp);

                    if tool_calls.is_empty() {
                        self.add_assistant(&resp);
                        let _ = events.send(AgentEvent::AssistantChunk(resp));
                        let _ = events.send(AgentEvent::TurnFinished {
                            usage: self.last_usage.clone(),
                            total_calls: self.total_calls,
                            duration_ms: started.elapsed().as_millis() as u64,
                            history: self.history.clone(),
                            total_usage: self.total_usage.clone(),
                            last_usage: self.last_usage.clone(),
                        });
                        return;
                    }

                    self.add_assistant(&resp);
                    let _ = events.send(AgentEvent::AssistantChunk(resp));

                    let mut tool_results = Vec::new();
                    for call in &tool_calls {
                        let args_str = serde_json::to_string(&call.args).unwrap_or_default();
                        let _ = events.send(AgentEvent::ToolStart {
                            tool: call.tool.clone(),
                            args: args_str,
                        });

                        let tool = call.clone();
                        let tool_cancel = cancel.clone();
                        let tool_name = tool.tool.clone();
                        let exec_result: Result<String> = tokio::select! {
                            _ = tool_cancel.cancelled() => Err(anyhow!("Cancelled by user")),
                            res = tokio::task::spawn_blocking(move || {
                                // exec_tool is a sync fn; call it on the blocking pool.
                                ReActAgent::exec_tool_static(&tool)
                            }) => match res {
                                Ok(inner) => inner,
                                Err(join_err) => Err(anyhow!("Join error: {}", join_err)),
                            },
                        };

                        match exec_result {
                            Ok(out) => {
                                let _ = events.send(AgentEvent::ToolOutput {
                                    tool: tool_name.clone(),
                                    output: out.clone(),
                                    is_error: false,
                                });
                                tool_results.push(format!("Tool '{}' output:\n{}", tool_name, out));
                            }
                            Err(e) => {
                                let _ = events.send(AgentEvent::ToolOutput {
                                    tool: tool_name.clone(),
                                    output: e.to_string(),
                                    is_error: true,
                                });
                                tool_results.push(format!("Tool '{}' error: {}", tool_name, e));
                            }
                        }
                    }

                    current_prompt = format!("Observation/Tool Results:\n{}", tool_results.join("\n\n"));
                }
                Err(e) => {
                    let _ = events.send(AgentEvent::Error(e.to_string()));
                    return;
                }
            }
        }

        // Exhausted step budget: emit whatever we have.
        let _ = events.send(AgentEvent::TurnFinished {
            usage: self.last_usage.clone(),
            total_calls: self.total_calls,
            duration_ms: started.elapsed().as_millis() as u64,
            history: self.history.clone(),
            total_usage: self.total_usage.clone(),
            last_usage: self.last_usage.clone(),
        });
    }

    /// `exec_tool` runs on the blocking pool; a small static shim lets the
    /// spawned task call it without needing a live `&self`.
    fn exec_tool_static(call: &ToolCall) -> Result<String> {
        let agent = Self {
            client: Client::new(),
            ..Default::default()
        };
        agent.exec_tool(call)
    }
}

pub async fn run() -> Result<()> {
    println!("CACode Sovereign ReAct Agent Chat REPL");
    println!("Type /help for slash commands or /exit to quit.\n");

    let mut line_editor = Reedline::create();
    let prompt = DefaultPrompt::default();
    let mut agent = ReActAgent::new();

    loop {
        let sig = line_editor.read_line(&prompt);
        match sig {
            Ok(Signal::Success(buffer)) => {
                if let Err(e) = agent.step(&buffer).await {
                    eprintln!("Error: {}", e);
                }
            }
            Ok(Signal::CtrlC) => {
                println!("\n(Interrupted. Type /exit to quit)");
            }
            Ok(Signal::CtrlD) => {
                println!("\nExiting CACode REPL.");
                break;
            }
            Ok(_) => {}
            Err(err) => {
                eprintln!("REPL Error: {}", err);
                break;
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_provider_antigravity() {
        env::remove_var("ANTHROPIC_API_KEY");
        env::remove_var("OPENAI_API_KEY");
        env::set_var("ANTIGRAVITY_API_KEY", "zen-test-key");
        env::set_var("ANTIGRAVITY_ZEN_MODEL", "mimo-v2.5-free");

        let p = ReActAgent::detect_provider();
        match p {
            Provider::AntigravityZen { model } => {
                assert_eq!(model, "mimo-v2.5-free");
            }
            _ => panic!("Expected Provider::AntigravityZen, got {:?}", p),
        }

        env::remove_var("ANTIGRAVITY_API_KEY");
        env::remove_var("ANTIGRAVITY_ZEN_MODEL");
    }

    #[test]
    fn test_extract_response_content_openai_format() {
        let json_body = r#"{
            "id": "chatcmpl-test",
            "choices": [
                {
                    "message": {
                        "role": "assistant",
                        "content": "Hello from Zen model!"
                    }
                }
            ],
            "usage": {
                "prompt_tokens": 12,
                "completion_tokens": 8,
                "total_tokens": 20
            }
        }"#;

        let (content, usage) = ReActAgent::extract_response_content(json_body).unwrap();
        assert_eq!(content, "Hello from Zen model!");
        assert_eq!(usage.prompt_tokens, 12);
        assert_eq!(usage.completion_tokens, 8);
        assert_eq!(usage.total_tokens, 20);
    }

    #[test]
    fn test_parse_tool_calls_json() {
        let agent = ReActAgent::new();
        let content = "```json\n[\n  {\"tool\": \"read_file\", \"args\": {\"path\": \"src/main.rs\"}}\n]\n```";
        let calls = agent.parse_tool_calls(content);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].tool, "read_file");
        assert_eq!(calls[0].args["path"], "src/main.rs");
    }

    #[test]
    fn test_parse_tool_calls_aider_search_replace_autodetect() {
        let agent = ReActAgent::new();
        let content = r#"
Here is the patch you requested:

crates/cacode-core/src/lib.rs
<<<<<<< SEARCH
pub mod commands;
=======
pub mod commands;
pub mod search_replace;
>>>>>>> REPLACE
"#;
        let calls = agent.parse_tool_calls(content);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].tool, "search_replace");
        assert_eq!(calls[0].args["path"], "crates/cacode-core/src/lib.rs");
        assert_eq!(calls[0].args["search"], "pub mod commands;\n");
        assert_eq!(calls[0].args["replace"], "pub mod commands;\npub mod search_replace;\n");
    }

    #[test]
    fn test_exec_tool_search_replace_and_patch_file() {
        let agent = ReActAgent::new();
        let temp_dir = std::env::temp_dir().join(format!("cacode_chat_test_{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        fs::create_dir_all(&temp_dir).unwrap();
        let test_file = temp_dir.join("test.txt");
        fs::write(&test_file, "hello   \nworld\n").unwrap();

        // Test patch_file with whitespace tolerance (content has trailing spaces on hello)
        let patch_call = ToolCall {
            tool: "patch_file".into(),
            args: serde_json::json!({
                "path": test_file.to_string_lossy(),
                "old_string": "hello\nworld\n",
                "new_string": "greetings\nworld\n",
            }),
        };
        let res = agent.exec_tool(&patch_call).unwrap();
        assert!(res.contains("Successfully patched"));
        assert_eq!(fs::read_to_string(&test_file).unwrap(), "greetings\nworld\n");

        // Test search_replace tool
        let sr_call = ToolCall {
            tool: "search_replace".into(),
            args: serde_json::json!({
                "path": test_file.to_string_lossy(),
                "search": "greetings",
                "replace": "welcome",
            }),
        };
        let res2 = agent.exec_tool(&sr_call).unwrap();
        assert!(res2.contains("Successfully applied search/replace"));
        assert_eq!(fs::read_to_string(&test_file).unwrap(), "welcome\nworld\n");

        let _ = fs::remove_dir_all(&temp_dir);
    }
}
