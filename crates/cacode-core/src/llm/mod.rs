//! Provider adapters: OpenAI-compatible (OpenAI, Ollama `/v1`, 9Router,
//! Antigravity Zen, DeepSeek, OpenRouter, vLLM, any custom endpoint) and
//! Anthropic Messages API. Streaming SSE, native tool calling, typed errors.

pub mod retry;
pub mod sse;

use crate::types::{Message, TokenUsage, ToolCall};
use crate::commands::chat::Provider;
use serde_json::{json, Value};
use std::time::Duration;

/// Typed provider failure — drives retry / failover decisions.
#[derive(Debug, Clone, PartialEq)]
pub enum LlmError {
    /// 429. `retry_after` from the header when present.
    RateLimited { retry_after: Option<Duration>, message: String },
    /// 5xx / 529 / overloaded.
    Unavailable(String),
    /// 401 / 403 / missing key. Never retried on the same provider.
    Auth(String),
    /// 400-class request problems. Never retried.
    BadRequest(String),
    /// Prompt exceeds the model context window → compact then retry.
    ContextOverflow(String),
    /// The endpoint rejected `tools` → switch the provider to text tool-mode.
    ToolsUnsupported(String),
    /// No first byte / stream went idle.
    Timeout(String),
    /// DNS / connect / reset.
    Network(String),
}

impl LlmError {
    pub fn retryable(&self) -> bool {
        matches!(self, Self::RateLimited { .. } | Self::Unavailable(_) | Self::Timeout(_) | Self::Network(_))
    }
    /// Worth trying the next provider in the fallback chain.
    pub fn failover(&self) -> bool {
        !matches!(self, Self::ContextOverflow(_) | Self::ToolsUnsupported(_))
    }
}

impl std::fmt::Display for LlmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RateLimited { retry_after, message } => write!(f, "rate limited (429{}): {}", retry_after.map(|d| format!(", retry after {}s", d.as_secs())).unwrap_or_default(), message),
            Self::Unavailable(m) => write!(f, "provider unavailable: {}", m),
            Self::Auth(m) => write!(f, "authentication failed: {}", m),
            Self::BadRequest(m) => write!(f, "bad request: {}", m),
            Self::ContextOverflow(m) => write!(f, "context window exceeded: {}", m),
            Self::ToolsUnsupported(m) => write!(f, "endpoint does not support native tools: {}", m),
            Self::Timeout(m) => write!(f, "timeout: {}", m),
            Self::Network(m) => write!(f, "network error: {}", m),
        }
    }
}
impl std::error::Error for LlmError {}

/// Endpoint + key resolved for a provider (no global env mutation).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Credentials {
    pub openai_key: Option<String>,
    pub openai_base: Option<String>,
    pub anthropic_key: Option<String>,
    pub anthropic_base: Option<String>,
    pub ollama_url: Option<String>,
    pub router9_url: Option<String>,
    pub router9_key: Option<String>,
    pub zen_key: Option<String>,
}

fn non_empty(v: Option<String>) -> Option<String> {
    v.filter(|s| !s.trim().is_empty())
}

impl Credentials {
    pub fn from_env() -> Self {
        let e = |k: &str| non_empty(std::env::var(k).ok());
        Self {
            openai_key: e("OPENAI_API_KEY"),
            openai_base: e("OPENAI_BASE_URL"),
            anthropic_key: e("ANTHROPIC_API_KEY"),
            anthropic_base: e("ANTHROPIC_BASE_URL"),
            ollama_url: e("OLLAMA_URL"),
            router9_url: e("ROUTER9_URL"),
            router9_key: e("ROUTER9_API_KEY"),
            zen_key: e("ANTIGRAVITY_API_KEY").or_else(|| e("ANTIGRAVITY_ZEN_API_KEY")),
        }
    }

    /// Config values win over environment variables.
    pub fn from_config(cfg: &crate::config::CaCodeConfig) -> Self {
        let env = Self::from_env();
        Self {
            openai_key: non_empty(cfg.openai_api_key.clone()).or(env.openai_key),
            openai_base: non_empty(cfg.openai_base_url.clone()).or(env.openai_base),
            anthropic_key: non_empty(cfg.anthropic_api_key.clone()).or(env.anthropic_key),
            anthropic_base: env.anthropic_base,
            ollama_url: non_empty(cfg.ollama_url.clone()).or(env.ollama_url),
            router9_url: non_empty(cfg.router9_url.clone()).or(env.router9_url),
            router9_key: non_empty(cfg.router9_api_key.clone()).or(env.router9_key),
            zen_key: non_empty(cfg.antigravity_zen_api_key.clone()).or(env.zen_key),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wire {
    OpenAi,
    Anthropic,
}

#[derive(Debug, Clone)]
pub struct Endpoint {
    pub wire: Wire,
    pub url: String,
    pub api_key: Option<String>,
    pub extra_headers: Vec<(String, String)>,
}

pub fn chat_completions_url(base: &str) -> String {
    let b = base.trim_end_matches('/');
    if b.ends_with("/chat/completions") {
        b.to_string()
    } else if b.ends_with("/v1") || b.ends_with("/api") || b.ends_with("/v1beta/openai") {
        format!("{}/chat/completions", b)
    } else {
        format!("{}/v1/chat/completions", b)
    }
}

pub fn resolve_endpoint(p: &Provider, c: &Credentials) -> Result<Endpoint, LlmError> {
    let openai = |url: String, key: Option<String>| {
        let mut extra = Vec::new();
        if url.contains("openrouter.ai") {
            extra.push(("HTTP-Referer".into(), "https://cecepazhar.com/cacode".into()));
            extra.push(("X-Title".into(), "CACode".into()));
        }
        Endpoint { wire: Wire::OpenAi, url, api_key: key, extra_headers: extra }
    };
    Ok(match p {
        Provider::OpenAI { .. } => {
            let key = c.openai_key.clone().ok_or_else(|| LlmError::Auth("OPENAI_API_KEY is not set".into()))?;
            openai(chat_completions_url(c.openai_base.as_deref().unwrap_or("https://api.openai.com")), Some(key))
        }
        Provider::Anthropic { .. } => {
            let key = c.anthropic_key.clone().ok_or_else(|| LlmError::Auth("ANTHROPIC_API_KEY is not set".into()))?;
            let base = c.anthropic_base.clone().unwrap_or_else(|| "https://api.anthropic.com".into());
            Endpoint { wire: Wire::Anthropic, url: format!("{}/v1/messages", base.trim_end_matches('/').trim_end_matches("/v1")), api_key: Some(key), extra_headers: vec![] }
        }
        Provider::Ollama { .. } => {
            let base = c.ollama_url.clone().unwrap_or_else(|| crate::provider_registry::DEFAULT_OLLAMA_URL.into());
            openai(format!("{}/v1/chat/completions", base.trim_end_matches('/').trim_end_matches("/v1")), None)
        }
        Provider::Router9 { .. } => openai(
            chat_completions_url(c.router9_url.as_deref().unwrap_or("http://localhost:20128")),
            Some(c.router9_key.clone().unwrap_or_else(|| "dummy".into())),
        ),
        Provider::AntigravityZen { .. } => openai(crate::provider_registry::ANTIGRAVITY_ZEN_ENDPOINT.to_string(), Some(c.zen_key.clone().unwrap_or_else(|| "dummy".into()))),
        Provider::Custom { base_url, api_key, .. } => openai(chat_completions_url(base_url), non_empty(Some(api_key.clone()))),
    })
}

/// A tool definition as sent to the provider.
#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub schema: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolMode {
    /// Provider-native function calling.
    Native,
    /// Model writes JSON tool calls in a ```json fence (legacy / weak models).
    Text,
}

pub struct LlmRequest<'a> {
    pub model: &'a str,
    pub system: &'a str,
    pub messages: &'a [Message],
    pub tools: &'a [ToolDef],
    pub tool_mode: ToolMode,
    pub max_tokens: u32,
    pub stream: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct LlmResponse {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: TokenUsage,
    pub stop_reason: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct Timeouts {
    /// Until response headers + first body byte.
    pub first_byte: Duration,
    /// Max silence between stream chunks.
    pub idle: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self { first_byte: Duration::from_secs(120), idle: Duration::from_secs(90) }
    }
}

pub fn build_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .pool_idle_timeout(Duration::from_secs(90))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

// ------------------------------------------------------- request bodies ----

fn render_text_tool_calls(calls: &[ToolCall]) -> String {
    let arr: Vec<Value> = calls.iter().map(|c| json!({"tool": c.tool, "args": c.args})).collect();
    format!("```json\n{}\n```", serde_json::to_string_pretty(&arr).unwrap_or_default())
}

fn tool_result_text(m: &Message) -> String {
    format!(
        "Observation from tool `{}`{}:\n{}",
        m.name.as_deref().unwrap_or("tool"),
        if m.is_error { " (ERROR)" } else { "" },
        m.content
    )
}

/// Merge consecutive messages with the same role (text mode / strict providers).
fn merge_same_role(msgs: Vec<Value>) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for m in msgs {
        if let Some(last) = out.last_mut() {
            if last["role"] == m["role"] && last["content"].is_string() && m["content"].is_string() && last.get("tool_calls").is_none() && m.get("tool_calls").is_none() {
                let merged = format!("{}\n\n{}", last["content"].as_str().unwrap_or(""), m["content"].as_str().unwrap_or(""));
                last["content"] = Value::String(merged);
                continue;
            }
        }
        out.push(m);
    }
    out
}

pub fn openai_body(req: &LlmRequest) -> Value {
    let mut msgs = vec![json!({"role": "system", "content": req.system})];
    for m in req.messages {
        match (m.role.as_str(), req.tool_mode) {
            ("system", _) => msgs.push(json!({"role": "system", "content": m.content})),
            ("tool", ToolMode::Native) => msgs.push(json!({"role": "tool", "tool_call_id": m.tool_call_id.clone().unwrap_or_default(), "content": m.content})),
            ("tool", ToolMode::Text) => msgs.push(json!({"role": "user", "content": tool_result_text(m)})),
            ("assistant", ToolMode::Native) if !m.tool_calls.is_empty() => {
                let calls: Vec<Value> = m
                    .tool_calls
                    .iter()
                    .map(|c| json!({"id": c.id, "type": "function", "function": {"name": c.tool, "arguments": serde_json::to_string(&c.args).unwrap_or_else(|_| "{}".into())}}))
                    .collect();
                msgs.push(json!({"role": "assistant", "content": if m.content.is_empty() { Value::Null } else { Value::String(m.content.clone()) }, "tool_calls": calls}));
            }
            ("assistant", ToolMode::Text) if !m.tool_calls.is_empty() && !m.content.contains("```json") => {
                msgs.push(json!({"role": "assistant", "content": format!("{}\n{}", m.content, render_text_tool_calls(&m.tool_calls)).trim().to_string()}));
            }
            (role, _) => msgs.push(json!({"role": role, "content": m.content})),
        }
    }
    let msgs = if req.tool_mode == ToolMode::Text { merge_same_role(msgs) } else { msgs };
    let mut body = json!({"model": req.model, "messages": msgs, "stream": req.stream});
    if req.stream {
        body["stream_options"] = json!({"include_usage": true});
    }
    if req.tool_mode == ToolMode::Native && !req.tools.is_empty() {
        body["tools"] = Value::Array(
            req.tools.iter().map(|t| json!({"type": "function", "function": {"name": t.name, "description": t.description, "parameters": t.schema}})).collect(),
        );
    }
    body
}

pub fn anthropic_body(req: &LlmRequest) -> Value {
    let mut msgs: Vec<Value> = Vec::new();
    let mut push = |role: &str, blocks: Vec<Value>| {
        if blocks.is_empty() {
            return;
        }
        if let Some(last) = msgs.last_mut() {
            if last["role"] == role {
                if let Some(arr) = last["content"].as_array_mut() {
                    arr.extend(blocks);
                    return;
                }
            }
        }
        msgs.push(json!({"role": role, "content": blocks}));
    };
    for m in req.messages {
        match m.role.as_str() {
            "system" => push("user", vec![json!({"type": "text", "text": format!("[system note] {}", m.content)})]),
            "tool" => push(
                "user",
                vec![json!({"type": "tool_result", "tool_use_id": m.tool_call_id.clone().unwrap_or_default(), "content": if m.content.is_empty() { "(empty)".to_string() } else { m.content.clone() }, "is_error": m.is_error})],
            ),
            "assistant" => {
                let mut blocks = Vec::new();
                if !m.content.trim().is_empty() {
                    blocks.push(json!({"type": "text", "text": m.content}));
                }
                for c in &m.tool_calls {
                    let input = if c.args.is_object() { c.args.clone() } else { json!({}) };
                    blocks.push(json!({"type": "tool_use", "id": c.id, "name": c.tool, "input": input}));
                }
                if blocks.is_empty() {
                    blocks.push(json!({"type": "text", "text": "(no content)"}));
                }
                push("assistant", blocks);
            }
            _ => push("user", vec![json!({"type": "text", "text": if m.content.is_empty() { "(empty)".to_string() } else { m.content.clone() }})]),
        }
    }
    if msgs.first().map(|m| m["role"] != "user").unwrap_or(true) {
        msgs.insert(0, json!({"role": "user", "content": [{"type": "text", "text": "(continue)"}]}));
    }
    // Prompt caching: breakpoints on the system prompt, the tool list and the
    // newest message, so each step re-reads the whole prefix from cache.
    if let Some(last) = msgs.last_mut() {
        if let Some(block) = last["content"].as_array_mut().and_then(|a| a.last_mut()) {
            block["cache_control"] = json!({"type": "ephemeral"});
        }
    }
    let system = json!([{"type": "text", "text": req.system, "cache_control": {"type": "ephemeral"}}]);
    let mut body = json!({"model": req.model, "max_tokens": req.max_tokens, "system": system, "messages": msgs, "stream": req.stream});
    if !req.tools.is_empty() {
        let mut tools: Vec<Value> = req.tools.iter().map(|t| json!({"name": t.name, "description": t.description, "input_schema": t.schema})).collect();
        if let Some(t) = tools.last_mut() {
            t["cache_control"] = json!({"type": "ephemeral"});
        }
        body["tools"] = Value::Array(tools);
    }
    body
}

// --------------------------------------------------------- error mapping ----

pub fn classify_status(status: u16, body: &str, retry_after: Option<Duration>) -> LlmError {
    let lower = body.to_lowercase();
    let msg = crate::text::truncate_chars(body.trim(), 500);
    let ctx_overflow = (lower.contains("context") && (lower.contains("length") || lower.contains("too long") || lower.contains("maximum") || lower.contains("exceed")))
        || lower.contains("prompt is too long")
        || lower.contains("too many tokens");
    let tools_unsupported = (lower.contains("tool") || lower.contains("function"))
        && (lower.contains("not support") || lower.contains("unsupported") || lower.contains("does not support") || lower.contains("unknown field") || lower.contains("unrecognized"));
    match status {
        401 | 403 => LlmError::Auth(msg),
        429 => LlmError::RateLimited { retry_after, message: msg },
        400 | 404 | 413 | 422 if ctx_overflow => LlmError::ContextOverflow(msg),
        400 | 404 | 422 if tools_unsupported => LlmError::ToolsUnsupported(msg),
        413 => LlmError::ContextOverflow(msg),
        400..=499 if status != 408 && status != 409 && status != 425 => LlmError::BadRequest(format!("HTTP {}: {}", status, msg)),
        _ => LlmError::Unavailable(format!("HTTP {}: {}", status, msg)),
    }
}

pub fn parse_retry_after(h: &reqwest::header::HeaderMap) -> Option<Duration> {
    let v = h.get("retry-after").or_else(|| h.get("x-ratelimit-reset-after"))?.to_str().ok()?.trim().to_string();
    if let Ok(secs) = v.parse::<f64>() {
        return Some(Duration::from_millis((secs.max(0.0) * 1000.0) as u64));
    }
    chrono::DateTime::parse_from_rfc2822(&v).ok().and_then(|t| (t.with_timezone(&chrono::Utc) - chrono::Utc::now()).to_std().ok())
}

fn map_reqwest(e: reqwest::Error) -> LlmError {
    if e.is_timeout() {
        LlmError::Timeout(e.to_string())
    } else {
        LlmError::Network(e.to_string())
    }
}

// ----------------------------------------------------------------- call ----

/// One request to one provider. `on_delta` receives streamed text.
pub async fn complete(
    client: &reqwest::Client,
    provider: &Provider,
    creds: &Credentials,
    req: &LlmRequest<'_>,
    timeouts: Timeouts,
    on_delta: &(dyn Fn(&str) + Send + Sync),
) -> Result<LlmResponse, LlmError> {
    let ep = resolve_endpoint(provider, creds)?;
    let mut body = match ep.wire {
        Wire::OpenAi => openai_body(req),
        Wire::Anthropic => anthropic_body(req),
    };
    // One protocol-level downgrade: some OpenAI-compatible servers reject stream_options.
    for attempt in 0..2 {
        let mut rb = client.post(&ep.url).json(&body);
        match (ep.wire, &ep.api_key) {
            (Wire::Anthropic, Some(k)) => rb = rb.header("x-api-key", k).header("anthropic-version", "2023-06-01"),
            (Wire::OpenAi, Some(k)) => rb = rb.bearer_auth(k),
            _ => {}
        }
        for (k, v) in &ep.extra_headers {
            rb = rb.header(k, v);
        }
        if req.stream {
            rb = rb.header("accept", "text/event-stream");
        }
        let resp = tokio::time::timeout(timeouts.first_byte, rb.send())
            .await
            .map_err(|_| LlmError::Timeout(format!("no response from {} within {}s", ep.url, timeouts.first_byte.as_secs())))?
            .map_err(map_reqwest)?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            let ra = parse_retry_after(resp.headers());
            let text = tokio::time::timeout(timeouts.idle, resp.text()).await.ok().and_then(|r| r.ok()).unwrap_or_default();
            if attempt == 0 && status == 400 && text.contains("stream_options") {
                body.as_object_mut().map(|o| o.remove("stream_options"));
                continue;
            }
            return Err(classify_status(status, &text, ra));
        }
        let is_sse = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).map(|v| v.contains("event-stream")).unwrap_or(false);
        let mut out = if is_sse {
            sse::read_stream(resp, ep.wire, timeouts.idle, on_delta).await?
        } else {
            let text = tokio::time::timeout(timeouts.idle, resp.text())
                .await
                .map_err(|_| LlmError::Timeout("response body stalled".into()))?
                .map_err(map_reqwest)?;
            let r = sse::parse_full_body(&text, ep.wire)?;
            if !r.content.is_empty() {
                on_delta(&r.content);
            }
            r
        };
        if out.usage.total_tokens == 0 {
            let prompt_chars: usize = req.system.len() + req.messages.iter().map(|m| m.content.len()).sum::<usize>();
            out.usage.prompt_tokens = (prompt_chars as f64 / 3.5).ceil() as i64;
            out.usage.completion_tokens = crate::text::estimate_tokens(&out.content).max(1)
                + out.tool_calls.iter().map(|c| crate::text::estimate_tokens(&c.args.to_string())).sum::<i64>();
            out.usage.total_tokens = out.usage.prompt_tokens + out.usage.completion_tokens;
            out.usage.estimated = true;
        }
        return Ok(out);
    }
    Err(LlmError::BadRequest("request rejected twice".into()))
}

#[cfg(test)]
mod tests;
