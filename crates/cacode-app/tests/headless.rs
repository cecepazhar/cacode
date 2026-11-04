//! End-to-end tests of the real `cacode` binary in headless mode against a
//! local mock OpenAI-compatible server (no network, isolated HOME).

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::sync::{Arc, Mutex};

/// Mock server on a thread. `reply(body) -> response json`.
fn mock(reply: impl Fn(&serde_json::Value) -> serde_json::Value + Send + Sync + 'static) -> (String, Arc<Mutex<Vec<serde_json::Value>>>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
    let seen: Arc<Mutex<Vec<serde_json::Value>>> = Arc::default();
    let s2 = seen.clone();
    std::thread::spawn(move || {
        for stream in l.incoming() {
            let Ok(mut st) = stream else { continue };
            let mut r = BufReader::new(st.try_clone().unwrap());
            let mut cl = 0usize;
            loop {
                let mut line = String::new();
                if r.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                if line.to_lowercase().starts_with("content-length:") {
                    cl = line[15..].trim().parse().unwrap_or(0);
                }
                if line == "\r\n" {
                    break;
                }
            }
            let mut body = vec![0u8; cl];
            let _ = r.read_exact(&mut body);
            let v: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
            s2.lock().unwrap().push(v.clone());
            let out = reply(&v).to_string();
            let _ = write!(st, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", out.len(), out);
        }
    });
    (url, seen)
}

fn text(t: &str) -> serde_json::Value {
    serde_json::json!({"choices": [{"message": {"role": "assistant", "content": t}}], "usage": {"prompt_tokens": 9, "completion_tokens": 4, "total_tokens": 13}})
}

fn cacode(home: &std::path::Path, ws: &std::path::Path, url: &str) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_cacode"));
    c.current_dir(ws).env("HOME", home).env("ROUTER9_URL", url).env("ROUTER9_MODEL", "mock-model").env("CACODE_NO_USAGE_DB", "1");
    for k in ["ANTHROPIC_API_KEY", "OPENAI_API_KEY", "ANTIGRAVITY_API_KEY", "ANTIGRAVITY_ZEN_API_KEY", "OLLAMA_URL", "OLLAMA_MODEL", "CACODE_PERMISSION_MODE"] {
        c.env_remove(k);
    }
    c
}

#[test]
fn headless_json_edit_then_continue_session() {
    let home = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    std::fs::write(ws.path().join("lib.rs"), "pub fn answer() -> i32 { 41 }\n").unwrap();
    let (url, seen) = mock(|b| {
        let msgs = b["messages"].as_array().cloned().unwrap_or_default();
        let last = msgs.last().cloned().unwrap_or_default();
        let user = msgs.iter().rev().find(|m| m["role"] == "user").map(|m| m["content"].as_str().unwrap_or("").to_string()).unwrap_or_default();
        let tool_msgs = msgs.iter().filter(|m| m["role"] == "tool").count();
        if user.contains("what did you change") {
            return text("I changed 41 to 42 in lib.rs");
        }
        match (last["role"].as_str(), tool_msgs) {
            (Some("user"), _) => serde_json::json!({"choices": [{"message": {"role": "assistant", "content": null, "tool_calls": [{"id": "a", "type": "function", "function": {"name": "read_file", "arguments": "{\"path\":\"lib.rs\"}"}}]}}], "usage": {"prompt_tokens": 9, "completion_tokens": 4, "total_tokens": 13}}),
            (Some("tool"), 1) => serde_json::json!({"choices": [{"message": {"role": "assistant", "content": null, "tool_calls": [{"id": "b", "type": "function", "function": {"name": "edit_file", "arguments": "{\"path\":\"lib.rs\",\"old_string\":\"41\",\"new_string\":\"42\"}"}}]}}], "usage": {"prompt_tokens": 9, "completion_tokens": 4, "total_tokens": 13}}),
            _ => text("Done: answer() now returns 42"),
        }
    });

    // ask mode (default) in headless → edit denied, file untouched, still exit 0 with explanation
    let out = cacode(home.path(), ws.path(), &url).args(["-p", "make answer return 42", "--output-format", "json"]).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let res: serde_json::Value = serde_json::from_str(stdout.lines().last().unwrap()).unwrap_or_else(|e| panic!("{e}: {stdout}\n{}", String::from_utf8_lossy(&out.stderr)));
    assert_eq!(res["permission_mode"], "ask");
    assert!(res["tool_calls"].as_array().unwrap().iter().any(|t| t["tool"] == "edit_file" && t["is_error"] == true), "{res}");
    assert_eq!(std::fs::read_to_string(ws.path().join("lib.rs")).unwrap(), "pub fn answer() -> i32 { 41 }\n");

    // auto-edit → applied
    let out = cacode(home.path(), ws.path(), &url).args(["-p", "make answer return 42", "--output-format", "json", "--permission-mode", "auto-edit"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let res: serde_json::Value = serde_json::from_str(String::from_utf8_lossy(&out.stdout).lines().last().unwrap()).unwrap();
    assert_eq!(res["outcome"], "finished");
    assert_eq!(res["result"], "Done: answer() now returns 42");
    assert_eq!(res["files_touched"][0], "lib.rs");
    assert_eq!(res["usage"]["total_tokens"], 39);
    assert_eq!(std::fs::read_to_string(ws.path().join("lib.rs")).unwrap(), "pub fn answer() -> i32 { 42 }\n");
    let sid = res["session_id"].as_str().unwrap().to_string();
    assert!(home.path().join(".cacode/sessions").join(format!("{sid}.json")).exists());

    // --continue sends the previous conversation along
    let before = seen.lock().unwrap().len();
    let out = cacode(home.path(), ws.path(), &url).args(["-p", "what did you change?", "-c"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "I changed 41 to 42 in lib.rs");
    let req = seen.lock().unwrap()[before].clone();
    let contents: Vec<String> = req["messages"].as_array().unwrap().iter().map(|m| m["content"].as_str().unwrap_or("").to_string()).collect();
    assert!(contents.iter().any(|c| c == "make answer return 42"), "previous turn missing: {contents:?}");
    assert!(req["messages"].as_array().unwrap().iter().any(|m| m["role"] == "tool"));

    // sessions listing
    let out = cacode(home.path(), ws.path(), &url).args(["sessions"]).output().unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains(&sid));
}

#[test]
fn headless_stream_json_and_exit_codes() {
    let home = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let (url, _) = mock(|_| serde_json::json!({"choices": [{"message": {"role": "assistant", "content": null, "tool_calls": [{"id": "x", "type": "function", "function": {"name": "list_dir", "arguments": "{}"}}]}}]}));
    // always calls a tool → step budget stop → exit code 2
    let out = cacode(home.path(), ws.path(), &url).args(["-p", "loop", "--output-format", "stream-json", "--max-steps", "2"]).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    let lines: Vec<serde_json::Value> = String::from_utf8_lossy(&out.stdout).lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert!(lines.iter().any(|l| l["type"] == "tool_start" && l["tool"] == "list_dir"));
    assert_eq!(lines.last().unwrap()["type"], "result");
    assert_eq!(lines.last().unwrap()["outcome"], "step_budget");
    assert_eq!(lines.last().unwrap()["usage"]["estimated"], true, "no usage from server → flagged as estimate");

    // unreachable provider → exit code 1 with a clear error
    let out = cacode(home.path(), ws.path(), "http://127.0.0.1:9").args(["-p", "hi", "--output-format", "json"]).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    let res: serde_json::Value = serde_json::from_str(String::from_utf8_lossy(&out.stdout).lines().last().unwrap()).unwrap();
    assert!(res["outcome"].as_str().unwrap().starts_with("failed: All providers failed"));
}
