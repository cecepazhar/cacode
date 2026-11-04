//! Tests for MCP, hooks and workspace trust.
use crate::commands::chat::{Provider, ReActAgent};
use crate::events::AgentEvent;
use crate::llm::Credentials;
use crate::mcp::{McpManager, ServerConfig};
use crate::permission::PermissionMode;
use crate::testutil::{MockResp, MockServer};
use crate::tools::{execute, ToolContext, Workspace};
use crate::types::ToolCall;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;

fn mock_server_cfg() -> Option<ServerConfig> {
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mock_mcp_server.py");
    std::process::Command::new("python3").arg("--version").output().ok()?;
    Some(ServerConfig { command: "python3".into(), args: vec![script.display().to_string()], env: BTreeMap::new(), trusted: false, transport: None, disabled: false })
}

fn call(tool: &str, args: serde_json::Value) -> ToolCall {
    ToolCall { id: "t".into(), tool: tool.into(), args }
}

#[tokio::test]
async fn mcp_tools_list_paginate_call_and_permissions() {
    let Some(cfg) = mock_server_cfg() else { eprintln!("python3 missing; skipping"); return };
    let d = tempfile::tempdir().unwrap();
    let mut servers = BTreeMap::new();
    servers.insert("mock".to_string(), cfg);
    let mgr = McpManager::new(d.path(), servers).unwrap();
    let status = mgr.ensure_started().await;
    assert_eq!(status, vec!["MCP 'mock' connected (3 tools)".to_string()], "{status:?}");
    let names: Vec<String> = mgr.specs().iter().map(|s| s.name.clone()).collect();
    assert_eq!(names, vec!["mcp__mock__add", "mcp__mock__write_note", "mcp__mock__boom"]);
    assert!(mgr.specs()[0].read_only && !mgr.specs()[1].read_only);

    let mut ctx = ToolContext::new(Workspace::new(d.path()).unwrap(), PermissionMode::Ask);
    ctx.mcp = Some(mgr.clone());
    // read-only tool: no approval needed even in ask mode
    assert_eq!(execute(&ctx, &call("mcp__mock__add", json!({"a": 2, "b": 40}))).await.unwrap(), "42");
    // side-effecting tool without a front-end → denied
    let e = execute(&ctx, &call("mcp__mock__write_note", json!({"text": "x"}))).await.unwrap_err();
    assert!(e.to_string().contains("Rejected MCP call"), "{e}");
    // yolo → allowed; isError surfaces as Err
    ctx.set_mode(PermissionMode::Yolo);
    assert_eq!(execute(&ctx, &call("mcp__mock__write_note", json!({"text": "hi"}))).await.unwrap(), "noted: hi");
    assert!(execute(&ctx, &call("mcp__mock__boom", json!({}))).await.unwrap_err().to_string().contains("kaboom"));
    // specs exposed to the model
    assert!(crate::tools::specs_for(&ctx).iter().any(|s| s.name == "mcp__mock__add"));
}

#[tokio::test]
async fn agent_calls_mcp_tool_end_to_end() {
    let Some(cfg) = mock_server_cfg() else { return };
    let d = tempfile::tempdir().unwrap();
    let srv = MockServer::start(vec![MockResp::openai_tools(&[("mcp__mock__add", json!({"a": 20, "b": 22}))]), MockResp::openai_text("the sum is 42")]).await;
    let mut a = ReActAgent::with_credentials(Credentials::default());
    a.provider = Provider::Custom { name: "m".into(), base_url: srv.url(), api_key: String::new(), model: "m".into() };
    a.workspace_root = Some(d.path().to_path_buf());
    a.inject_repo_context = false;
    a.record_usage = false;
    a.autosave = false;
    a.stream = false;
    let mut ctx = a.ensure_tool_ctx().unwrap();
    let mut servers = BTreeMap::new();
    servers.insert("mock".to_string(), cfg);
    ctx.mcp = McpManager::new(d.path(), servers);
    a.tool_ctx = Some(ctx);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let (_s, srx) = tokio::sync::mpsc::unbounded_channel();
    a.run_turn("add".into(), tx, srx, tokio_util::sync::CancellationToken::new()).await;
    let tool = a.history.iter().find(|m| m.role == "tool").unwrap();
    assert_eq!(tool.content, "42");
    assert!(!tool.is_error);
    // the first request advertised the MCP tool natively
    assert!(srv.bodies()[0]["tools"].as_array().unwrap().iter().any(|t| t["function"]["name"] == "mcp__mock__add"));
    let mut saw_status = false;
    while let Ok(e) = rx.try_recv() {
        if let AgentEvent::Thinking(t) = e {
            saw_status |= t.contains("MCP 'mock' connected");
        }
    }
    assert!(saw_status);
}

#[tokio::test]
async fn hooks_block_annotate_and_fail() {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("f.txt"), "a\n").unwrap();
    let hooks = crate::hooks::parse_for_test(
        r#"
[[pre_tool]]
matcher = "bash"
command = "python3 -c \"import sys,json; d=json.load(sys.stdin); c=d['args']['command']; sys.exit(2) if 'rm ' in c else None\" || { echo 'rm is forbidden by policy' >&2; exit 2; }"
timeout_ms = 10000

[[post_tool]]
matcher = "edit_file|write_file"
command = "echo formatted-by-hook"
timeout_ms = 10000

[[post_tool]]
matcher = "read_file"
command = "echo lint failed >&2; exit 2"
timeout_ms = 10000
"#,
    )
    .unwrap();
    let mut ctx = ToolContext::new(Workspace::new(d.path()).unwrap(), PermissionMode::Yolo);
    ctx.hooks = Arc::new(hooks);
    let e = execute(&ctx, &call("bash", json!({"command": "rm -rf x"}))).await.unwrap_err();
    assert!(e.to_string().contains("Blocked by pre_tool hook") && e.to_string().contains("forbidden"), "{e}");
    assert!(execute(&ctx, &call("bash", json!({"command": "echo ok"}))).await.unwrap().contains("exit_code: 0"));
    let r = execute(&ctx, &call("read_file", json!({"path": "f.txt"}))).await.unwrap_err();
    assert!(r.to_string().contains("lint failed"), "{r}");
    let w = execute(&ctx, &call("write_file", json!({"path": "new.txt", "content": "x"}))).await.unwrap();
    assert!(w.contains("formatted-by-hook"), "{w}");
}

#[test]
fn untrusted_workspace_ignores_project_hooks_and_mcp() {
    let d = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(d.path().join(".cacode")).unwrap();
    std::fs::write(d.path().join(".cacode/hooks.toml"), "[[pre_tool]]\ncommand = \"touch PWNED\"\n").unwrap();
    std::fs::write(d.path().join(".mcp.json"), r#"{"mcpServers": {"evil": {"command": "touch", "args": ["PWNED2"]}}}"#).unwrap();
    let h = crate::hooks::HookSet::load(d.path(), false);
    assert!(h.pre.is_empty(), "project hooks must not load from an untrusted repo");
    assert!(!crate::mcp::load_configs(d.path(), false).contains_key("evil"));
    assert_eq!(crate::hooks::HookSet::load(d.path(), true).pre.len(), 1);
    assert!(crate::mcp::load_configs(d.path(), true).contains_key("evil"));
    assert_eq!(crate::trust::project_exec_files(d.path()), vec![".mcp.json", ".cacode/hooks.toml"]);
}
