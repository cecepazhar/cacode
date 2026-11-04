//! Incremental Server-Sent-Events parsing for OpenAI-compatible and Anthropic
//! streams, plus a parser for complete (non-streamed) JSON bodies.

use super::{LlmError, LlmResponse, Wire};
use crate::types::{TokenUsage, ToolCall};
use futures_util::StreamExt;
use serde_json::Value;
use std::collections::BTreeMap;
use std::time::Duration;

#[derive(Default)]
struct PartialCall {
    id: String,
    name: String,
    args: String,
}

/// Accumulates stream events into an [`LlmResponse`]. Public for tests and
/// for the monitor proxy's usage tee.
#[derive(Default)]
pub struct StreamAccumulator {
    wire_anthropic: bool,
    pub content: String,
    calls: BTreeMap<usize, PartialCall>,
    pub usage: TokenUsage,
    pub stop_reason: Option<String>,
    pub error: Option<LlmError>,
    pub model: Option<String>,
}

fn usage_from(u: &Value, into: &mut TokenUsage) {
    let get = |k: &str| u.get(k).and_then(|v| v.as_i64());
    if let Some(p) = get("prompt_tokens").or_else(|| get("input_tokens")) {
        // Anthropic reports cache tokens separately from input_tokens;
        // OpenAI includes cached tokens in prompt_tokens (details.cached_tokens).
        let read = get("cache_read_input_tokens").unwrap_or(0);
        let write = get("cache_creation_input_tokens").unwrap_or(0);
        let openai_cached = u.pointer("/prompt_tokens_details/cached_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
        into.prompt_tokens = p + read + write;
        into.cache_read_tokens = read + openai_cached;
        into.cache_write_tokens = write;
    }
    if let Some(c) = get("completion_tokens").or_else(|| get("output_tokens")) {
        into.completion_tokens = c;
    }
    into.total_tokens = get("total_tokens").filter(|t| *t >= into.prompt_tokens + into.completion_tokens).unwrap_or(into.prompt_tokens + into.completion_tokens);
}

fn error_from(v: &Value) -> Option<LlmError> {
    let e = v.get("error")?;
    let msg = e.get("message").and_then(|m| m.as_str()).map(String::from).unwrap_or_else(|| e.to_string());
    let typ = e.get("type").and_then(|t| t.as_str()).unwrap_or("");
    Some(match typ {
        "overloaded_error" | "api_error" | "server_error" => LlmError::Unavailable(msg),
        "rate_limit_error" => LlmError::RateLimited { retry_after: None, message: msg },
        "authentication_error" | "permission_error" => LlmError::Auth(msg),
        _ => super::classify_status(400, &msg, None),
    })
}

impl StreamAccumulator {
    pub fn new(wire: Wire) -> Self {
        Self { wire_anthropic: wire == Wire::Anthropic, ..Default::default() }
    }

    /// Feed one `data:` payload. Returns the text delta, if any.
    pub fn feed(&mut self, data: &str) -> Option<String> {
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            return None;
        }
        let v: Value = serde_json::from_str(data).ok()?;
        if let Some(e) = error_from(&v) {
            self.error = Some(e);
            return None;
        }
        if let Some(m) = v.get("model").and_then(|m| m.as_str()) {
            self.model.get_or_insert_with(|| m.to_string());
        }
        if self.wire_anthropic {
            self.feed_anthropic(&v)
        } else {
            self.feed_openai(&v)
        }
    }

    fn feed_openai(&mut self, v: &Value) -> Option<String> {
        if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
            usage_from(u, &mut self.usage);
        }
        let choice = v.get("choices")?.as_array()?.first()?;
        if let Some(r) = choice.get("finish_reason").and_then(|r| r.as_str()) {
            self.stop_reason = Some(r.to_string());
        }
        let delta = choice.get("delta").or_else(|| choice.get("message"))?;
        if let Some(tcs) = delta.get("tool_calls").and_then(|t| t.as_array()) {
            for (pos, tc) in tcs.iter().enumerate() {
                let idx = tc.get("index").and_then(|i| i.as_u64()).map(|i| i as usize).unwrap_or(pos);
                let e = self.calls.entry(idx).or_default();
                if let Some(id) = tc.get("id").and_then(|i| i.as_str()) {
                    e.id = id.to_string();
                }
                if let Some(f) = tc.get("function") {
                    if let Some(n) = f.get("name").and_then(|n| n.as_str()) {
                        e.name.push_str(n);
                    }
                    match f.get("arguments") {
                        Some(Value::String(a)) => e.args.push_str(a),
                        Some(obj @ Value::Object(_)) => e.args = obj.to_string(),
                        _ => {}
                    }
                }
            }
        }
        let text = delta.get("content").and_then(|c| c.as_str()).filter(|s| !s.is_empty())?;
        self.content.push_str(text);
        Some(text.to_string())
    }

    fn feed_anthropic(&mut self, v: &Value) -> Option<String> {
        match v.get("type").and_then(|t| t.as_str()).unwrap_or("") {
            "message_start" => {
                if let Some(u) = v.pointer("/message/usage") {
                    usage_from(u, &mut self.usage);
                }
                if let Some(m) = v.pointer("/message/model").and_then(|m| m.as_str()) {
                    self.model = Some(m.to_string());
                }
                None
            }
            "content_block_start" => {
                let idx = v.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                let b = v.get("content_block")?;
                if b.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                    let e = self.calls.entry(idx).or_default();
                    e.id = b.get("id").and_then(|i| i.as_str()).unwrap_or_default().to_string();
                    e.name = b.get("name").and_then(|i| i.as_str()).unwrap_or_default().to_string();
                }
                None
            }
            "content_block_delta" => {
                let idx = v.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                let d = v.get("delta")?;
                match d.get("type").and_then(|t| t.as_str()) {
                    Some("text_delta") => {
                        let t = d.get("text").and_then(|t| t.as_str())?;
                        self.content.push_str(t);
                        Some(t.to_string())
                    }
                    Some("input_json_delta") => {
                        if let Some(p) = d.get("partial_json").and_then(|p| p.as_str()) {
                            self.calls.entry(idx).or_default().args.push_str(p);
                        }
                        None
                    }
                    _ => None,
                }
            }
            "message_delta" => {
                if let Some(u) = v.get("usage") {
                    if let Some(o) = u.get("output_tokens").and_then(|o| o.as_i64()) {
                        self.usage.completion_tokens = o;
                        self.usage.total_tokens = self.usage.prompt_tokens + o;
                    }
                }
                if let Some(r) = v.pointer("/delta/stop_reason").and_then(|r| r.as_str()) {
                    self.stop_reason = Some(r.to_string());
                }
                None
            }
            _ => None,
        }
    }

    pub fn finish(self) -> Result<LlmResponse, LlmError> {
        if let Some(e) = self.error {
            return Err(e);
        }
        let mut tool_calls = Vec::new();
        for (i, c) in self.calls.into_values().enumerate() {
            if c.name.is_empty() {
                continue;
            }
            let args = if c.args.trim().is_empty() {
                Value::Object(Default::default())
            } else {
                serde_json::from_str(&c.args).unwrap_or_else(|e| serde_json::json!({"__invalid_json__": c.args, "__parse_error__": e.to_string()}))
            };
            let id = if c.id.is_empty() { format!("call_{}", i + 1) } else { c.id };
            tool_calls.push(ToolCall { id, tool: c.name, args });
        }
        Ok(LlmResponse { content: self.content, tool_calls, usage: self.usage, stop_reason: self.stop_reason })
    }
}

/// Split a byte buffer into complete SSE events (`\n\n` separated); returns
/// the `data:` payloads and leaves any partial event in `buf`.
pub fn drain_events(buf: &mut String) -> Vec<String> {
    let mut out = Vec::new();
    loop {
        let normalized_end = buf.find("\n\n").map(|i| (i, 2)).or_else(|| buf.find("\r\n\r\n").map(|i| (i, 4)));
        let Some((end, sep)) = normalized_end else { break };
        let event: String = buf.drain(..end + sep).collect();
        let data: Vec<&str> = event.lines().filter_map(|l| l.strip_prefix("data:")).map(|d| d.strip_prefix(' ').unwrap_or(d)).collect();
        if !data.is_empty() {
            out.push(data.join("\n"));
        }
    }
    out
}

pub async fn read_stream(
    resp: reqwest::Response,
    wire: Wire,
    idle: Duration,
    on_delta: &(dyn Fn(&str) + Send + Sync),
) -> Result<LlmResponse, LlmError> {
    let mut acc = StreamAccumulator::new(wire);
    let mut stream = resp.bytes_stream();
    let mut buf = String::new();
    let mut pending_utf8: Vec<u8> = Vec::new();
    loop {
        let next = tokio::time::timeout(idle, stream.next())
            .await
            .map_err(|_| LlmError::Timeout(format!("stream idle for {}s", idle.as_secs())))?;
        let Some(chunk) = next else { break };
        let chunk = chunk.map_err(|e| LlmError::Network(e.to_string()))?;
        pending_utf8.extend_from_slice(&chunk);
        match std::str::from_utf8(&pending_utf8) {
            Ok(s) => {
                buf.push_str(s);
                pending_utf8.clear();
            }
            Err(e) => {
                let valid = e.valid_up_to();
                buf.push_str(std::str::from_utf8(&pending_utf8[..valid]).unwrap_or_default());
                pending_utf8.drain(..valid);
            }
        }
        for data in drain_events(&mut buf) {
            if let Some(d) = acc.feed(&data) {
                on_delta(&d);
            }
        }
    }
    buf.push_str("\n\n");
    for data in drain_events(&mut buf) {
        if let Some(d) = acc.feed(&data) {
            on_delta(&d);
        }
    }
    acc.finish()
}

/// Parse a non-streamed JSON body (or a buffered SSE transcript).
pub fn parse_full_body(text: &str, wire: Wire) -> Result<LlmResponse, LlmError> {
    let trimmed = text.trim();
    if trimmed.starts_with("data:") || trimmed.contains("\ndata:") {
        let mut acc = StreamAccumulator::new(wire);
        let mut buf = format!("{}\n\n", trimmed);
        for d in drain_events(&mut buf) {
            acc.feed(&d);
        }
        return acc.finish();
    }
    let v: Value = serde_json::from_str(trimmed).map_err(|_| LlmError::BadRequest(format!("invalid response from model: {}", crate::text::truncate_chars(trimmed, 200))))?;
    if let Some(e) = error_from(&v) {
        return Err(e);
    }
    let mut usage = TokenUsage::default();
    if let Some(u) = v.get("usage") {
        usage_from(u, &mut usage);
    }
    if wire == Wire::Anthropic || v.get("content").map(|c| c.is_array()).unwrap_or(false) {
        let mut content = String::new();
        let mut tool_calls = Vec::new();
        for b in v.get("content").and_then(|c| c.as_array()).cloned().unwrap_or_default() {
            match b.get("type").and_then(|t| t.as_str()) {
                Some("text") => content.push_str(b.get("text").and_then(|t| t.as_str()).unwrap_or("")),
                Some("tool_use") => tool_calls.push(ToolCall {
                    id: b.get("id").and_then(|i| i.as_str()).unwrap_or_default().into(),
                    tool: b.get("name").and_then(|i| i.as_str()).unwrap_or_default().into(),
                    args: b.get("input").cloned().unwrap_or(Value::Null),
                }),
                _ => {}
            }
        }
        return Ok(LlmResponse { content, tool_calls, usage, stop_reason: v.get("stop_reason").and_then(|s| s.as_str()).map(String::from) });
    }
    // OpenAI-compatible, or Ollama native (`message` at top level)
    let msg = v.pointer("/choices/0/message").or_else(|| v.get("message"));
    let Some(msg) = msg else {
        return Err(LlmError::BadRequest(format!("invalid response from model: {}", crate::text::truncate_chars(trimmed, 200))));
    };
    if usage.total_tokens == 0 {
        if let (Some(p), Some(c)) = (v.get("prompt_eval_count").and_then(|x| x.as_i64()), v.get("eval_count").and_then(|x| x.as_i64())) {
            usage = TokenUsage { prompt_tokens: p, completion_tokens: c, total_tokens: p + c, ..Default::default() };
        }
    }
    let mut acc = StreamAccumulator::new(Wire::OpenAi);
    acc.feed(&serde_json::json!({"choices": [{"delta": msg, "finish_reason": v.pointer("/choices/0/finish_reason")}]}).to_string());
    let mut r = acc.finish()?;
    r.usage = usage;
    Ok(r)
}
