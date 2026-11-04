use super::*;
use crate::commands::chat::Provider;
use crate::testutil::{MockResp, MockServer};
use serde_json::json;

fn custom(srv: &MockServer, name: &str) -> Provider {
    Provider::Custom { name: name.into(), base_url: srv.url(), api_key: "k".into(), model: "mock-model".into() }
}

fn req<'a>(msgs: &'a [Message], tools: &'a [ToolDef]) -> LlmRequest<'a> {
    LlmRequest { model: "mock-model", system: "sys", messages: msgs, tools, tool_mode: ToolMode::Native, max_tokens: 100, stream: true }
}

#[tokio::test]
async fn openai_stream_text_and_tool_calls_and_usage() {
    let srv = MockServer::start(vec![MockResp::sse(&[
        json!({"choices":[{"delta":{"content":"Hel"}}]}),
        json!({"choices":[{"delta":{"content":"lo"}}]}),
        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_a","function":{"name":"read_file","arguments":"{\"pa"}}]}}]}),
        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"x.rs\"}"}}]},"finish_reason":"tool_calls"}]}),
        json!({"choices":[],"usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}}),
    ])])
    .await;
    let deltas = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let d2 = deltas.clone();
    let msgs = vec![Message::user("hi")];
    let r = complete(&build_client(), &custom(&srv, "m"), &Credentials::default(), &req(&msgs, &[]), Timeouts::default(), &move |d: &str| d2.lock().unwrap().push(d.to_string()))
        .await
        .unwrap();
    assert_eq!(r.content, "Hello");
    assert_eq!(*deltas.lock().unwrap(), vec!["Hel", "lo"]);
    assert_eq!(r.tool_calls.len(), 1);
    assert_eq!(r.tool_calls[0].id, "call_a");
    assert_eq!(r.tool_calls[0].args["path"], "x.rs");
    assert_eq!(r.usage.prompt_tokens, 42);
    assert!(!r.usage.estimated);
    let body = &srv.bodies()[0];
    assert_eq!(body["stream"], true);
    assert_eq!(body["stream_options"]["include_usage"], true);
}

#[tokio::test]
async fn anthropic_stream_tool_use() {
    let srv = MockServer::start(vec![MockResp::sse(&[
        json!({"type":"message_start","message":{"model":"claude-x","usage":{"input_tokens":30,"output_tokens":1}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Checking"}}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"grep","input":{}}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"pattern\":"}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"fn main\"}"}}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":12}}),
    ])])
    .await;
    let creds = Credentials { anthropic_key: Some("k".into()), anthropic_base: Some(srv.url()), ..Default::default() };
    let msgs = vec![Message::user("hi")];
    let r = complete(&build_client(), &Provider::Anthropic { model: "claude-x".into() }, &creds, &req(&msgs, &[]), Timeouts::default(), &|_: &str| {}).await.unwrap();
    assert_eq!(r.content, "Checking");
    assert_eq!(r.tool_calls[0].tool, "grep");
    assert_eq!(r.tool_calls[0].args["pattern"], "fn main");
    assert_eq!(r.usage.prompt_tokens, 30);
    assert_eq!(r.usage.completion_tokens, 12);
    assert_eq!(srv.requests.lock().unwrap()[0].path, "/v1/messages");
}

#[test]
fn anthropic_body_pairs_tool_use_and_results() {
    let call = ToolCall { id: "toolu_1".into(), tool: "read_file".into(), args: json!({"path": "a"}) };
    let call2 = ToolCall { id: "toolu_2".into(), tool: "grep".into(), args: json!({"pattern": "x"}) };
    let mut a = Message::assistant("");
    a.tool_calls = vec![call.clone(), call2.clone()];
    let msgs = vec![Message::user("q"), a, Message::tool_result(&call, "A", false), Message::tool_result(&call2, "B", true), Message::user("steer")];
    let body = anthropic_body(&req(&msgs, &[]));
    let m = body["messages"].as_array().unwrap();
    assert_eq!(m.len(), 3, "{:#}", body);
    assert_eq!(m[1]["content"][0]["type"], "tool_use");
    assert_eq!(m[2]["role"], "user");
    assert_eq!(m[2]["content"][0]["tool_use_id"], "toolu_1");
    assert_eq!(m[2]["content"][1]["is_error"], true);
    assert_eq!(m[2]["content"][2]["text"], "steer");
}

#[test]
fn openai_body_text_mode_merges_observations() {
    let call = ToolCall { id: "c1".into(), tool: "bash".into(), args: json!({"command": "ls"}) };
    let mut a = Message::assistant("```json\n[{\"tool\":\"bash\",\"args\":{\"command\":\"ls\"}}]\n```");
    a.tool_calls = vec![call.clone()];
    let msgs = vec![Message::user("q"), a, Message::tool_result(&call, "OUT", false), Message::user("more")];
    let mut r = req(&msgs, &[]);
    r.tool_mode = ToolMode::Text;
    let body = openai_body(&r);
    let m = body["messages"].as_array().unwrap();
    let roles: Vec<&str> = m.iter().map(|x| x["role"].as_str().unwrap()).collect();
    assert_eq!(roles, vec!["system", "user", "assistant", "user"]);
    assert!(m[3]["content"].as_str().unwrap().contains("Observation from tool `bash`"));
    assert!(m[3]["content"].as_str().unwrap().contains("more"));
    assert!(body.get("tools").is_none());
}

#[test]
fn status_classification() {
    assert!(matches!(classify_status(401, "bad key", None), LlmError::Auth(_)));
    assert!(matches!(classify_status(429, "slow", Some(Duration::from_secs(2))), LlmError::RateLimited { retry_after: Some(_), .. }));
    assert!(matches!(classify_status(503, "down", None), LlmError::Unavailable(_)));
    assert!(matches!(classify_status(529, "overloaded", None), LlmError::Unavailable(_)));
    assert!(matches!(classify_status(400, "This model's maximum context length is 8192 tokens", None), LlmError::ContextOverflow(_)));
    assert!(matches!(classify_status(400, "prompt is too long: 210000 tokens > 200000 maximum", None), LlmError::ContextOverflow(_)));
    assert!(matches!(classify_status(400, "model does not support tools", None), LlmError::ToolsUnsupported(_)));
    assert!(matches!(classify_status(400, "invalid json", None), LlmError::BadRequest(_)));
    assert!(!LlmError::Auth("x".into()).retryable());
    assert!(LlmError::Unavailable("x".into()).retryable());
}

#[tokio::test]
async fn first_byte_timeout() {
    let srv = MockServer::start(vec![MockResp::openai_text("late").delayed(Duration::from_secs(5))]).await;
    let msgs = vec![Message::user("hi")];
    let t = Timeouts { first_byte: Duration::from_millis(300), idle: Duration::from_millis(300) };
    let started = std::time::Instant::now();
    let e = complete(&build_client(), &custom(&srv, "m"), &Credentials::default(), &req(&msgs, &[]), t, &|_: &str| {}).await.unwrap_err();
    assert!(matches!(e, LlmError::Timeout(_)), "{e:?}");
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn stream_options_rejected_is_downgraded() {
    let srv = MockServer::start(vec![MockResp::status(400, "{\"error\":{\"message\":\"unknown field stream_options\"}}"), MockResp::openai_text("ok")]).await;
    let msgs = vec![Message::user("hi")];
    let r = complete(&build_client(), &custom(&srv, "m"), &Credentials::default(), &req(&msgs, &[]), Timeouts::default(), &|_: &str| {}).await.unwrap();
    assert_eq!(r.content, "ok");
    assert!(srv.bodies()[1].get("stream_options").is_none());
}

#[test]
fn retry_after_parsing() {
    let mut h = reqwest::header::HeaderMap::new();
    h.insert("retry-after", "2".parse().unwrap());
    assert_eq!(parse_retry_after(&h), Some(Duration::from_secs(2)));
    let p = retry::RetryPolicy::default();
    assert_eq!(p.delay(1, &LlmError::RateLimited { retry_after: Some(Duration::from_secs(2)), message: String::new() }), Duration::from_secs(2));
    let d1 = p.delay(1, &LlmError::Unavailable(String::new()));
    let d4 = p.delay(4, &LlmError::Unavailable(String::new()));
    assert!(d1 < Duration::from_millis(400) && d4 >= Duration::from_millis(1400), "{d1:?} {d4:?}");
    assert!(p.delay(30, &LlmError::Unavailable(String::new())) <= Duration::from_secs(8));
}

#[test]
fn endpoint_resolution() {
    let c = Credentials { ollama_url: Some("http://127.0.0.1:11434".into()), ..Default::default() };
    assert_eq!(resolve_endpoint(&Provider::Ollama { model: "x".into() }, &c).unwrap().url, "http://127.0.0.1:11434/v1/chat/completions");
    assert!(matches!(resolve_endpoint(&Provider::OpenAI { model: "x".into() }, &Credentials::default()), Err(LlmError::Auth(_))));
    let or = Provider::Custom { name: "OR".into(), base_url: "https://openrouter.ai/api/v1".into(), api_key: "k".into(), model: "x".into() };
    let ep = resolve_endpoint(&or, &c).unwrap();
    assert_eq!(ep.url, "https://openrouter.ai/api/v1/chat/completions");
    assert!(ep.extra_headers.iter().any(|(k, _)| k == "X-Title"));
    let ds = Provider::Custom { name: "DS".into(), base_url: "https://api.deepseek.com".into(), api_key: "k".into(), model: "deepseek-chat".into() };
    assert_eq!(resolve_endpoint(&ds, &c).unwrap().url, "https://api.deepseek.com/v1/chat/completions");
}

#[test]
fn anthropic_prompt_caching_breakpoints() {
    let tools = vec![ToolDef { name: "a".into(), description: "d".into(), schema: json!({"type": "object"}) }, ToolDef { name: "b".into(), description: "d".into(), schema: json!({"type": "object"}) }];
    let msgs = vec![Message::user("one"), Message::assistant("two"), Message::user("three")];
    let body = anthropic_body(&req(&msgs, &tools));
    assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
    assert!(body["tools"][0].get("cache_control").is_none());
    assert_eq!(body["tools"][1]["cache_control"]["type"], "ephemeral");
    let m = body["messages"].as_array().unwrap();
    assert_eq!(m.last().unwrap()["content"][0]["cache_control"]["type"], "ephemeral");
    assert!(m[0]["content"][0].get("cache_control").is_none(), "only the newest message is a breakpoint");
    let n: usize = body.to_string().matches("cache_control").count();
    assert!(n <= 4, "Anthropic allows at most 4 breakpoints, got {n}");
}

#[test]
fn cache_usage_parsed_and_priced() {
    let r = sse::parse_full_body(r#"{"content":[{"type":"text","text":"x"}],"usage":{"input_tokens":100,"cache_read_input_tokens":9000,"cache_creation_input_tokens":1000,"output_tokens":50}}"#, Wire::Anthropic).unwrap();
    assert_eq!(r.usage.prompt_tokens, 10_100);
    assert_eq!(r.usage.cache_read_tokens, 9000);
    assert_eq!(r.usage.cache_write_tokens, 1000);
    let c = crate::pricing::cost_usage_usd("Anthropic", "claude-sonnet-4-20250514", &r.usage).unwrap();
    // (100 + 9000*0.1 + 1000*1.25) * 3/1M + 50 * 15/1M
    let expected = (100.0 + 900.0 + 1250.0) * 3.0 / 1e6 + 50.0 * 15.0 / 1e6;
    assert!((c - expected).abs() < 1e-12, "{c} vs {expected}");
    let o = sse::parse_full_body(r#"{"choices":[{"message":{"content":"x"}}],"usage":{"prompt_tokens":2000,"completion_tokens":10,"total_tokens":2010,"prompt_tokens_details":{"cached_tokens":1500}}}"#, Wire::OpenAi).unwrap();
    assert_eq!(o.usage.prompt_tokens, 2000);
    assert_eq!(o.usage.cache_read_tokens, 1500);
}
