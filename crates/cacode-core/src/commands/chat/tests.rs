use super::*;
use crate::testutil::{MockResp, MockServer};
use serde_json::json;

fn agent_for(srv: &MockServer, dir: &std::path::Path) -> ReActAgent {
    std::env::set_var("CACODE_NO_USAGE_DB", "1");
    let mut a = ReActAgent::with_credentials(Credentials::default());
    a.provider = Provider::Custom { name: "mock".into(), base_url: srv.url(), api_key: "k".into(), model: "mock-model".into() };
    a.workspace_root = Some(dir.to_path_buf());
    a.permission_mode = PermissionMode::Yolo;
    a.inject_repo_context = false;
    a.record_usage = false;
    a.autosave = false;
    a.stream = false;
    a.retry.base = Duration::from_millis(10);
    a
}

async fn run(a: &mut ReActAgent, prompt: &str) -> Vec<AgentEvent> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (_s, srx) = mpsc::unbounded_channel();
    a.run_turn(prompt.into(), tx, srx, CancellationToken::new()).await;
    let mut evs = Vec::new();
    while let Ok(e) = rx.try_recv() {
        evs.push(e);
    }
    evs
}

fn roles(body: &serde_json::Value) -> Vec<String> {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            let c = if m["role"] == "assistant" { "" } else { m["content"].as_str().unwrap_or("") };
            let tag = if c.contains("OBS_MARKER_1") { ":OBS1" } else if c.contains("OBS_MARKER_2") { ":OBS2" } else if c.contains("HELLO_USER") { ":USER" } else { "" };
            format!("{}{}", m["role"].as_str().unwrap(), tag)
        })
        .collect()
}

/// Regression for audit F-01: observations must persist and the user prompt must not duplicate.
#[tokio::test]
async fn react_loop_keeps_every_observation_native() {
    let d = tempfile::tempdir().unwrap();
    let srv = MockServer::start(vec![
        MockResp::openai_tools(&[("bash", json!({"command": "echo OBS_MARKER_1"}))]),
        MockResp::openai_tools(&[("bash", json!({"command": "echo OBS_MARKER_2"}))]),
        MockResp::openai_text("final answer"),
    ])
    .await;
    let mut a = agent_for(&srv, d.path());
    let evs = run(&mut a, "HELLO_USER").await;
    assert!(evs.iter().any(|e| matches!(e, AgentEvent::TurnFinished { .. })));
    let b = srv.bodies();
    assert_eq!(roles(&b[0]), vec!["system", "user:USER"]);
    assert_eq!(roles(&b[1]), vec!["system", "user:USER", "assistant", "tool:OBS1"]);
    assert_eq!(roles(&b[2]), vec!["system", "user:USER", "assistant", "tool:OBS1", "assistant", "tool:OBS2"]);
    assert_eq!(b[2]["messages"][3]["tool_call_id"], b[2]["messages"][2]["tool_calls"][0]["id"]);
    let hist: Vec<&str> = a.history.iter().map(|m| m.role.as_str()).collect();
    assert_eq!(hist, vec!["user", "assistant", "tool", "assistant", "tool", "assistant"]);
    assert!(b[0]["tools"].as_array().unwrap().iter().any(|t| t["function"]["name"] == "edit_file"));
}

#[tokio::test]
async fn text_mode_downgrade_when_tools_rejected() {
    let d = tempfile::tempdir().unwrap();
    let srv = MockServer::start(vec![
        MockResp::status(400, "{\"error\":{\"message\":\"this model does not support tools\"}}"),
        MockResp::openai_text("```json\n[{\"tool\":\"bash\",\"args\":{\"command\":\"echo OBS_MARKER_1\"}}]\n```"),
        MockResp::openai_text("done"),
    ])
    .await;
    let mut a = agent_for(&srv, d.path());
    run(&mut a, "HELLO_USER").await;
    let b = srv.bodies();
    assert!(b[0].get("tools").is_some());
    assert!(b[1].get("tools").is_none(), "second request must be text mode");
    assert!(b[1]["messages"][0]["content"].as_str().unwrap().contains("Tool calling (text mode)"));
    assert_eq!(roles(&b[2]), vec!["system", "user:USER", "assistant", "user:OBS1"]);
}

#[tokio::test]
async fn auth_error_is_not_retried_and_fails_over() {
    let d = tempfile::tempdir().unwrap();
    let bad = MockServer::start(vec![MockResp::status(401, "{\"error\":{\"message\":\"invalid api key\"}}")]).await;
    let good = MockServer::start(vec![MockResp::openai_text("from fallback")]).await;
    let mut a = agent_for(&bad, d.path());
    a.fallback_chain = vec![Provider::Custom { name: "good".into(), base_url: good.url(), api_key: "k".into(), model: "m".into() }];
    let evs = run(&mut a, "hi").await;
    assert_eq!(bad.count(), 1, "401 must not be retried");
    assert_eq!(good.count(), 1);
    assert!(evs.iter().any(|e| matches!(e, AgentEvent::Thinking(t) if t.contains("Failing over"))));
    assert_eq!(a.history.last().unwrap().content, "from fallback");
    assert!(a.provider.name() == "mock", "primary must not be replaced permanently");
}

#[tokio::test]
async fn rate_limit_honours_retry_after() {
    let d = tempfile::tempdir().unwrap();
    let srv = MockServer::start(vec![MockResp::status(429, "{\"error\":{\"message\":\"slow down\"}}").header("retry-after", "1"), MockResp::openai_text("ok")]).await;
    let mut a = agent_for(&srv, d.path());
    run(&mut a, "hi").await;
    let r = srv.requests.lock().unwrap().clone();
    assert_eq!(r.len(), 2);
    let gap = r[1].at.duration_since(r[0].at);
    assert!(gap >= Duration::from_millis(950), "waited only {gap:?}");
}

#[tokio::test]
async fn breaker_skips_failed_primary_then_recovers_after_cooldown() {
    let d = tempfile::tempdir().unwrap();
    let primary = MockServer::start(vec![
        MockResp::status(503, "down"),
        MockResp::status(503, "down"),
        MockResp::status(503, "down"),
        MockResp::openai_text("primary back"),
    ])
    .await;
    let fb = MockServer::start(vec![MockResp::openai_text("fallback")]).await;
    let mut a = agent_for(&primary, d.path());
    a.retry.cooldown = Duration::from_millis(400);
    a.fallback_chain = vec![Provider::Custom { name: "fb".into(), base_url: fb.url(), api_key: "k".into(), model: "m".into() }];
    run(&mut a, "one").await;
    assert_eq!(primary.count(), 3);
    assert_eq!(a.history.last().unwrap().content, "fallback");
    run(&mut a, "two").await; // breaker open → fallback first
    assert_eq!(primary.count(), 3);
    assert_eq!(fb.count(), 2);
    tokio::time::sleep(Duration::from_millis(450)).await;
    run(&mut a, "three").await;
    assert_eq!(primary.count(), 4);
    assert_eq!(a.history.last().unwrap().content, "primary back");
}

#[tokio::test]
async fn hung_provider_times_out_and_fails_over() {
    let d = tempfile::tempdir().unwrap();
    let hung = MockServer::start(vec![MockResp::openai_text("never").delayed(Duration::from_secs(30))]).await;
    let fb = MockServer::start(vec![MockResp::openai_text("rescued")]).await;
    let mut a = agent_for(&hung, d.path());
    a.max_retries = 1;
    a.timeouts = Timeouts { first_byte: Duration::from_millis(300), idle: Duration::from_millis(300) };
    a.fallback_chain = vec![Provider::Custom { name: "fb".into(), base_url: fb.url(), api_key: "k".into(), model: "m".into() }];
    let t0 = Instant::now();
    run(&mut a, "hi").await;
    assert!(t0.elapsed() < Duration::from_secs(3));
    assert_eq!(a.history.last().unwrap().content, "rescued");
}

#[tokio::test]
async fn tool_errors_are_observations_not_crashes() {
    let d = tempfile::tempdir().unwrap();
    let srv = MockServer::start(vec![
        MockResp::openai_tools(&[("read_file", json!({"path": "/etc/shadow"})), ("no_such_tool", json!({}))]),
        MockResp::openai_text("recovered"),
    ])
    .await;
    let mut a = agent_for(&srv, d.path());
    run(&mut a, "hi").await;
    let tools: Vec<&Message> = a.history.iter().filter(|m| m.role == "tool").collect();
    assert_eq!(tools.len(), 2);
    assert!(tools.iter().all(|m| m.is_error));
    assert!(tools[0].content.contains("PathOutsideWorkspace"));
    assert!(tools[1].content.contains("Unknown tool"));
    assert_eq!(a.history.last().unwrap().content, "recovered");
}

#[tokio::test]
async fn loop_guard_and_step_budget() {
    let d = tempfile::tempdir().unwrap();
    let srv = MockServer::start(vec![MockResp::openai_tools(&[("list_dir", json!({"path": "."}))])]).await;
    let mut a = agent_for(&srv, d.path());
    a.max_steps = 10;
    let evs = run(&mut a, "hi").await;
    assert!(srv.count() <= 5, "loop guard should stop early, made {} calls", srv.count());
    assert!(evs.iter().any(|e| matches!(e, AgentEvent::AssistantChunk(t) if t.contains("repeated"))));
}

#[tokio::test]
async fn cancel_mid_tool_leaves_consistent_history() {
    let d = tempfile::tempdir().unwrap();
    let srv = MockServer::start(vec![MockResp::openai_tools(&[("bash", json!({"command": "sleep 30"})), ("bash", json!({"command": "echo second"}))])]).await;
    let mut a = agent_for(&srv, d.path());
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (_s, srx) = mpsc::unbounded_channel();
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(600)).await;
        c2.cancel();
    });
    let t0 = Instant::now();
    a.run_turn("hi".into(), tx, srx, cancel).await;
    assert!(t0.elapsed() < Duration::from_secs(5));
    let mut got_sync = false;
    let mut got_cancel = false;
    while let Ok(e) = rx.try_recv() {
        match e {
            AgentEvent::HistorySync { history, .. } => {
                got_sync = true;
                let calls = history.iter().find(|m| !m.tool_calls.is_empty()).unwrap().tool_calls.len();
                let results = history.iter().filter(|m| m.role == "tool").count();
                assert_eq!(calls, results, "every tool_call needs a result so the next request is valid");
            }
            AgentEvent::Cancelled => got_cancel = true,
            _ => {}
        }
    }
    assert!(got_sync && got_cancel);
}

#[tokio::test]
async fn auto_compaction_keeps_request_under_window() {
    let d = tempfile::tempdir().unwrap();
    let srv = MockServer::start_fn(std::sync::Arc::new(|b: &serde_json::Value| {
        if b.to_string().contains("You compress coding-agent conversations") {
            MockResp::openai_text("[COMPACTED CONTEXT SUMMARY] user asked about X")
        } else {
            MockResp::openai_text("answer")
        }
    }))
    .await;
    let mut a = agent_for(&srv, d.path());
    a.context_window_override = Some(20_000);
    for i in 0..30 {
        a.history.push(Message::user(format!("question {i} {}", "lorem ipsum ".repeat(250))));
        a.history.push(Message::assistant(format!("answer {i} {}", "dolor sit ".repeat(250))));
    }
    let before = a.estimate_prompt_tokens("");
    assert!(before > 20_000);
    let evs = run(&mut a, "latest question").await;
    assert!(evs.iter().any(|e| matches!(e, AgentEvent::Compacted(_))), "should auto-compact");
    let after = a.estimate_prompt_tokens("");
    assert!(after < 16_000, "after={after}");
    assert!(a.history[0].content.contains("[COMPACTED CONTEXT SUMMARY]"));
    assert!(a.history.iter().any(|m| m.content == "latest question"), "recent tail must survive");
    let b = srv.bodies();
    let final_req_chars: usize = b.last().unwrap()["messages"].as_array().unwrap().iter().map(|m| m["content"].as_str().unwrap_or("").len()).sum();
    assert!((final_req_chars as f64 / 3.5) < 20_000.0);
}

#[tokio::test]
async fn context_overflow_from_provider_triggers_compaction() {
    let d = tempfile::tempdir().unwrap();
    let srv = MockServer::start(vec![
        MockResp::status(400, "{\"error\":{\"message\":\"This model's maximum context length is 8192 tokens\"}}"),
        MockResp::openai_text("[COMPACTED CONTEXT SUMMARY] s"),
        MockResp::openai_text("ok after compaction"),
    ])
    .await;
    let mut a = agent_for(&srv, d.path());
    for i in 0..12 {
        a.history.push(Message::user(format!("q{i}")));
        a.history.push(Message::assistant(format!("a{i}")));
    }
    run(&mut a, "now").await;
    assert_eq!(a.history.last().unwrap().content, "ok after compaction");
}

#[tokio::test]
async fn parallel_subagents_respect_semaphore_and_report_evidence() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let d = tempfile::tempdir().unwrap();
    let in_flight = std::sync::Arc::new(AtomicUsize::new(0));
    let peak = std::sync::Arc::new(AtomicUsize::new(0));
    let (inf, pk) = (in_flight.clone(), peak.clone());
    let srv = MockServer::start_fn(std::sync::Arc::new(move |b: &serde_json::Value| {
        let msgs = b["messages"].as_array().cloned().unwrap_or_default();
        let last = msgs.last().cloned().unwrap_or_default();
        let first_user = msgs.iter().find(|m| m["role"] == "user").map(|m| m["content"].as_str().unwrap_or("").to_string()).unwrap_or_default();
        if first_user.starts_with("run probe") {
            if last["role"] == "tool" {
                inf.fetch_sub(1, Ordering::SeqCst);
                MockResp::openai_text("```json\n{\"summary\": \"probe passed\", \"claims\": [\"tests pass\"]}\n```")
            } else {
                let now = inf.fetch_add(1, Ordering::SeqCst) + 1;
                pk.fetch_max(now, Ordering::SeqCst);
                MockResp::openai_tools(&[("bash", json!({"command": "echo probe-ok"}))]).delayed(Duration::from_millis(250))
            }
        } else if last["role"] == "tool" {
            MockResp::openai_text("parent done")
        } else {
            let task = |n: usize| ("task", json!({"description": format!("probe {n}"), "prompt": format!("run probe {n}")}));
            MockResp::openai_tools(&[task(1), task(2), task(3), task(4)])
        }
    }))
    .await;
    let mut a = agent_for(&srv, d.path());
    a.subagent_limit = 2;
    let t0 = Instant::now();
    let evs = run(&mut a, "investigate").await;
    let reports: Vec<&Message> = a.history.iter().filter(|m| m.role == "tool" && m.name.as_deref() == Some("task")).collect();
    assert_eq!(reports.len(), 4);
    for r in &reports {
        assert!(r.content.starts_with("SUBAGENT REPORT\n"), "{}", r.content);
        assert!(r.content.contains("\"verified\": true"));
        assert!(r.content.contains("echo probe-ok"), "evidence must come from the exec log");
    }
    assert_eq!(peak.load(Ordering::SeqCst), 2, "semaphore must cap concurrency at 2");
    assert!(t0.elapsed() >= Duration::from_millis(500), "4 tasks / 2 slots → ≥2 waves");
    let starts = evs.iter().filter(|e| matches!(e, AgentEvent::Subagent { status: crate::events::SubagentStatus::Started, .. })).count();
    assert_eq!(starts, 4);
    assert_eq!(a.history.last().unwrap().content, "parent done");
    // subagents never get the `task` tool
    for b in srv.bodies() {
        let fu = b["messages"].as_array().unwrap().iter().find(|m| m["role"] == "user").map(|m| m["content"].as_str().unwrap_or("").to_string()).unwrap_or_default();
        if fu.starts_with("run probe") {
            assert!(!b["tools"].as_array().unwrap().iter().any(|t| t["function"]["name"] == "task"));
        }
    }
}

#[tokio::test]
async fn subagent_claim_without_evidence_is_flagged() {
    let d = tempfile::tempdir().unwrap();
    let srv = MockServer::start(vec![
        MockResp::openai_tools(&[("task", json!({"description": "fix", "prompt": "fix it"}))]),
        MockResp::openai_text("```json\n{\"summary\": \"All tests pass, bug fixed\", \"claims\": []}\n```"),
        MockResp::openai_text("ok"),
    ])
    .await;
    let mut a = agent_for(&srv, d.path());
    run(&mut a, "go").await;
    let r = a.history.iter().find(|m| m.name.as_deref() == Some("task")).unwrap();
    assert!(r.content.contains("UNVERIFIED"), "{}", r.content);
    assert!(r.content.contains("\"verified\": false"));
}

#[tokio::test]
async fn edit_mode_subagent_refused_in_ask_mode() {
    let d = tempfile::tempdir().unwrap();
    let srv = MockServer::start(vec![MockResp::openai_tools(&[("task", json!({"description": "e", "prompt": "p", "mode": "edit"}))]), MockResp::openai_text("ok")]).await;
    let mut a = agent_for(&srv, d.path());
    a.permission_mode = PermissionMode::Ask;
    run(&mut a, "go").await;
    let r = a.history.iter().find(|m| m.role == "tool").unwrap();
    assert!(r.is_error && r.content.contains("auto-edit or yolo"), "{}", r.content);
}

#[test]
fn parse_tool_calls_json_and_aider() {
    let agent = ReActAgent::with_credentials(Credentials::default());
    let calls = agent.parse_tool_calls("```json\n[\n  {\"tool\": \"read_file\", \"args\": {\"path\": \"src/main.rs\"}}\n]\n```");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].args["path"], "src/main.rs");
    let single = agent.parse_tool_calls("```json\n{\"tool\": \"grep\", \"args\": {\"pattern\": \"x\"}}\n```");
    assert_eq!(single[0].tool, "grep");
    let sr = agent.parse_tool_calls("crates/a/lib.rs\n<<<<<<< SEARCH\npub mod commands;\n=======\npub mod commands;\npub mod x;\n>>>>>>> REPLACE\n");
    assert_eq!(sr[0].tool, "search_replace");
    assert_eq!(sr[0].args["path"], "crates/a/lib.rs");
}

#[test]
fn custom_provider_display_and_serde() {
    let p = Provider::Custom { name: "Xiaomi DeepSeek".into(), base_url: "https://api.x/v1".into(), api_key: "xm".into(), model: "deepseek-v3".into() };
    assert_eq!(p.display_label(), "Xiaomi DeepSeek (deepseek-v3)");
    let back: Provider = serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
    assert_eq!(back, p);
}

#[test]
fn detect_provider_prefers_config_keys() {
    let c = Credentials { zen_key: Some("z".into()), ..Default::default() };
    assert!(matches!(ReActAgent::detect_provider_with(&c), Provider::AntigravityZen { .. }));
    let c = Credentials { anthropic_key: Some("a".into()), zen_key: Some("z".into()), ..Default::default() };
    assert!(matches!(ReActAgent::detect_provider_with(&c), Provider::Anthropic { .. }));
}
