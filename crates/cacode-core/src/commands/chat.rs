use anyhow::{anyhow, Result};
use reedline::{DefaultPrompt, Reedline, Signal};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::process::Command;
use walkdir::WalkDir;
use regex::Regex;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Deserialize)]
struct OpenAIChoice {
    message: Message,
}

#[derive(Debug, Deserialize)]
struct OpenAIResponse {
    choices: Vec<OpenAIChoice>,
}

#[derive(Debug, Deserialize)]
struct AnthropicResponse {
    content: Vec<AnthropicContent>,
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
}

#[derive(Debug, Deserialize)]
pub struct ToolCall {
    pub tool: String,
    pub args: serde_json::Value,
}

pub enum Provider {
    OpenAI { model: String },
    Anthropic { model: String },
    Ollama { model: String },
    Router9 { model: String },
}

pub struct ReActAgent {
    client: Client,
    pub history: Vec<Message>,
    pub provider: Provider,
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
        }
    }

    pub fn add_user(&mut self, content: &str) {
        self.history.push(Message { role: "user".into(), content: content.into() });
    }

    pub fn add_assistant(&mut self, content: &str) {
        self.history.push(Message { role: "assistant".into(), content: content.into() });
    }

    pub fn system_prompt() -> Message {
        Message {
            role: "system".into(),
            content: "You are CACode, a sovereign Rust-focused development agent.
Available tools:
1. read_file: {\"tool\": \"read_file\", \"args\": {\"path\": \"...\", \"offset\": 1, \"limit\": 200}}
2. patch_file: {\"tool\": \"patch_file\", \"args\": {\"path\": \"...\", \"old_string\": \"...\", \"new_string\": \"...\"}}
3. write_file: {\"tool\": \"write_file\", \"args\": {\"path\": \"...\", \"content\": \"...\"}}
4. bash: {\"tool\": \"bash\", \"args\": {\"command\": \"...\"}}
5. search_files: {\"tool\": \"search_files\", \"args\": {\"pattern\": \"...\", \"dir\": \".\"}}

When you need to act, output a JSON array of tool calls strictly wrapped in ```json ... ``` codeblock:
```json
[
  {\"tool\": \"read_file\", \"args\": {\"path\": \"src/main.rs\"}}
]
```
If no tool action is needed, reply normally with natural language.".into(),
        }
    }

    pub async fn call_llm(&self, user_input: &str) -> Result<String> {
        let mut msgs = vec![Self::system_prompt()];
        msgs.extend(self.history.clone());
        msgs.push(Message { role: "user".into(), content: user_input.into() });

        match &self.provider {
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
                    }))
                    .send()
                    .await?
                    .json::<OpenAIResponse>()
                    .await?;
                Ok(resp.choices.into_iter().next().ok_or_else(|| anyhow!("No choices returned from OpenAI"))?.message.content)
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
                        "system": Self::system_prompt().content,
                        "messages": non_system_msgs,
                    }))
                    .send()
                    .await?
                    .json::<AnthropicResponse>()
                    .await?;
                let txt = resp.content.into_iter().filter_map(|c| c.text).collect::<Vec<_>>().join("\n");
                Ok(txt)
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
                Ok(resp.message.content)
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
                let parsed: serde_json::Value = serde_json::from_str(&text)
                    .map_err(|_| anyhow!("Invalid JSON from 9router ({}): {}", text.chars().take(200).collect::<String>(), status))?;
                let err_msg = parsed.get("error").and_then(|e| e.get("message")).and_then(|m| m.as_str());
                if let Some(msg) = err_msg {
                    return Err(anyhow!("9router error: {}", msg));
                }
                let v: OpenAIResponse = serde_json::from_value(parsed)?;
                Ok(v.choices.into_iter().next().ok_or_else(|| anyhow!("No choices returned from 9router"))?.message.content)
            }
        }
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
                if !content.contains(old_str) {
                    return Err(anyhow!("Target old_string not found in {}", path));
                }
                let patched = content.replacen(old_str, new_str, 1);
                fs::write(path, patched)?;
                Ok(format!("Successfully patched {}", path))
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
