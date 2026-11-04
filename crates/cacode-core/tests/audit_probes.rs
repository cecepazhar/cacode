use cacode_core::commands::chat::*;
use cacode_core::apply_search_replace;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};


fn repo_root_exec(call: ToolCall) -> String {
    use cacode_core::tools::{execute, ToolContext, Workspace};
    let ctx = ToolContext::new(Workspace::new("../..").unwrap(), cacode_core::permission::PermissionMode::Yolo);
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(execute(&ctx, &call)).unwrap()
}

#[test]
fn probe_ambiguous_exact_match_patches_first_silently() {
    let c = "fn a() {\n    x += 1;\n}\nfn b() {\n    x += 1;\n}\n";
    let r = apply_search_replace(c, "    x += 1;\n", "    x += 2;\n");
    println!("AMBIG RESULT: {:?}", r);
    assert!(r.is_err());
}

#[test]
fn probe_fuzzy_patches_wrong_location() {
    let c = "let timeout_ms = 500;\nlet retries = 3;\n";
    // search for a line that does NOT exist
    let r = apply_search_replace(c, "let timeout_secs = 30;\n", "DELETED\n");
    println!("FUZZY RESULT: {:?}", r);
    assert!(r.is_err());
}

#[test]
fn probe_empty_search_prepends() {
    let r = apply_search_replace("fn main() {}\n", "", "GARBAGE\n");
    println!("EMPTY SEARCH RESULT: {:?}", r);
    assert!(r.is_err());
}

#[test]
fn probe_bash_no_truncation() {
    let a = ReActAgent::new();
    let out = a.exec_tool(&ToolCall{ id: String::new(), tool:"bash".into(), args: serde_json::json!({"command":"head -c 3000000 /dev/zero | tr '\\0' a"})}).unwrap();
    println!("BASH OUTPUT BYTES: {}", out.len());
    assert!(out.len() <= 30_200);
}

#[test]
fn probe_search_files_no_ignore() {
    let out = repo_root_exec(ToolCall{ id: String::new(), tool:"search_files".into(), args: serde_json::json!({"pattern":"fn main","dir":"."})});
    let total = out.lines().count();
    let in_target = out.lines().filter(|l| l.contains("/target/")).count();
    println!("SEARCH HITS total={total} in_target={in_target}");
    assert_eq!(in_target, 0);
}

#[test]
fn probe_path_escape() {
    let a = ReActAgent::new();
    let r = a.exec_tool(&ToolCall{ id: String::new(), tool:"read_file".into(), args: serde_json::json!({"path":"/etc/hostname"})});
    println!("READ OUTSIDE WORKSPACE: ok={}", r.is_ok());
    assert!(r.is_err());
}

async fn mock_server(bodies: Arc<Mutex<Vec<String>>>, replies: Vec<String>) -> u16 {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        let mut i = 0;
        loop {
            let (mut s, _) = l.accept().await.unwrap();
            let mut buf = vec![0u8; 1<<20]; let mut n = 0;
            loop {
                let k = s.read(&mut buf[n..]).await.unwrap(); n += k;
                let t = String::from_utf8_lossy(&buf[..n]).to_string();
                if let Some(h) = t.find("\r\n\r\n") {
                    let cl = t.lines().find(|l| l.to_lowercase().starts_with("content-length:"))
                        .and_then(|l| l[15..].trim().parse::<usize>().ok()).unwrap_or(0);
                    if n >= h + 4 + cl { bodies.lock().unwrap().push(t[h+4..].to_string()); break; }
                }
                if k == 0 { break; }
            }
            let content = replies.get(i).cloned().unwrap_or("done".into()); i += 1;
            let body = serde_json::json!({"choices":[{"message":{"role":"assistant","content":content}}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}).to_string();
            let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
            s.write_all(resp.as_bytes()).await.unwrap();
        }
    });
    port
}

#[tokio::test]
async fn probe_react_loop_message_sequence() {
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let tc = "```json\n[{\"tool\":\"bash\",\"args\":{\"command\":\"echo OBS_MARKER_1\"}}]\n```".to_string();
    let tc2 = "```json\n[{\"tool\":\"bash\",\"args\":{\"command\":\"echo OBS_MARKER_2\"}}]\n```".to_string();
    let port = mock_server(bodies.clone(), vec![tc, tc2, "final answer".into()]).await;
    std::env::set_var("CACODE_NO_USAGE_DB", "1");
    let mut a = ReActAgent::new();
    a.permission_mode = cacode_core::permission::PermissionMode::Yolo;
    a.inject_repo_context = false;
    a.stream = false;
    a.autosave = false;
    a.provider = Provider::Custom{ name:"mock".into(), base_url: format!("http://127.0.0.1:{port}"), api_key:"".into(), model:"m".into()};
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let (_stx, srx) = tokio::sync::mpsc::unbounded_channel();
    a.run_turn("HELLO_USER".into(), tx, srx, tokio_util::sync::CancellationToken::new()).await;
    while let Ok(_e) = rx.try_recv() {}
    for (i, b) in bodies.lock().unwrap().iter().enumerate() {
        let v: serde_json::Value = serde_json::from_str(b).unwrap();
        let seq: Vec<String> = v["messages"].as_array().unwrap().iter().map(|m| {
            let c = m["content"].as_str().unwrap_or("");
            let c = if m["role"] == "assistant" { "" } else { c };
            let tag = if c.contains("OBS_MARKER_1") && m["role"] == "tool" {"OBS1"} else if c.contains("OBS_MARKER_2") && m["role"] == "tool" {"OBS2"} else if c.contains("HELLO_USER") {"USER"} else if m["role"]=="system" {"SYS"} else {"asst"};
            format!("{}:{}", m["role"].as_str().unwrap(), tag)
        }).collect();
        println!("CALL {}: {:?}", i+1, seq);
    }
    let roles: Vec<String> = a.history.iter().map(|m| m.role.clone()).collect();
    println!("FINAL HISTORY ROLES: {:?}", roles);
    assert_eq!(roles, vec!["user", "assistant", "tool", "assistant", "tool", "assistant"]);
}
#[test]
fn probe_search_files_target() {
    let t = std::time::Instant::now();
    let out = repo_root_exec(ToolCall{ id: String::new(), tool:"search_files".into(), args: serde_json::json!({"pattern":"cacode","dir":"."})});
    let in_target = out.lines().filter(|l| l.contains("target/")).count();
    println!("SEARCH2 total={} in_target={} ms={}", out.lines().count(), in_target, t.elapsed().as_millis());
    assert_eq!(in_target, 0);
}
