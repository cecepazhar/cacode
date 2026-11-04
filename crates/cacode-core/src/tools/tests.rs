use super::*;
use crate::events::{AgentEvent, ApprovalDecision};
use serde_json::json;
use std::time::{Duration, Instant};

fn ctx_in(dir: &std::path::Path, mode: PermissionMode) -> ToolContext {
    let mut c = ToolContext::new(Workspace::new(dir).unwrap(), mode);
    c.undo = Arc::new(UndoStore::at(dir.join(".undo-test")));
    c
}

fn call(tool: &str, args: serde_json::Value) -> ToolCall {
    ToolCall { id: "t".into(), tool: tool.into(), args }
}

#[tokio::test]
async fn read_is_numbered_paginated_and_sandboxed() {
    let d = tempfile::tempdir().unwrap();
    let body: String = (1..=3000).map(|i| format!("line {i}\n")).collect();
    std::fs::write(d.path().join("big.txt"), body).unwrap();
    std::fs::write(d.path().join("bin.dat"), [0u8, 1, 2, 3]).unwrap();
    let c = ctx_in(d.path(), PermissionMode::Yolo);
    let out = execute(&c, &call("read_file", json!({"path": "big.txt"}))).await.unwrap();
    assert!(out.starts_with("[big.txt] lines 1-2000 of 3000 — more available, call read_file with offset=2001"), "{}", &out[..120]);
    assert!(out.contains("     1\tline 1\n"));
    let out2 = execute(&c, &call("read_file", json!({"path": "big.txt", "offset": 2999, "limit": 5}))).await.unwrap();
    assert!(out2.contains("lines 2999-3000 of 3000"));
    assert!(execute(&c, &call("read_file", json!({"path": "bin.dat"}))).await.unwrap_err().to_string().contains("binary"));
    let e = execute(&c, &call("read_file", json!({"path": "/etc/hostname"}))).await.unwrap_err();
    assert!(e.to_string().contains("PathOutsideWorkspace"));
    let e = execute(&c, &call("read_file", json!({"path": "../../../etc/passwd"}))).await.unwrap_err();
    assert!(e.to_string().contains("PathOutsideWorkspace"));
}

#[tokio::test]
async fn edit_requires_read_and_detects_stale_file_then_undo() {
    let d = tempfile::tempdir().unwrap();
    let f = d.path().join("a.rs");
    std::fs::write(&f, "fn a() {\n    1\n}\n").unwrap();
    let c = ctx_in(d.path(), PermissionMode::Yolo);
    let edit = call("edit_file", json!({"path": "a.rs", "old_string": "    1\n", "new_string": "    2\n"}));
    let e = execute(&c, &edit).await.unwrap_err();
    assert!(e.to_string().contains("NotReadYet"), "{e}");
    execute(&c, &call("read_file", json!({"path": "a.rs"}))).await.unwrap();
    // user edits the file in their editor meanwhile
    std::fs::write(&f, "fn a() {\n    1\n}\n// user change\n").unwrap();
    let e = execute(&c, &edit).await.unwrap_err();
    assert!(e.to_string().contains("StaleFile"), "{e}");
    let orig_hash = fs::sha256_hex(&std::fs::read(&f).unwrap());
    execute(&c, &call("read_file", json!({"path": "a.rs"}))).await.unwrap();
    let ok = execute(&c, &edit).await.unwrap();
    assert!(ok.contains("Updated a.rs") && ok.contains("+    2"), "{ok}");
    assert!(std::fs::read_to_string(&f).unwrap().contains("    2"));
    let msg = c.undo.undo_last().unwrap();
    assert!(msg.contains("Restored"));
    assert_eq!(fs::sha256_hex(&std::fs::read(&f).unwrap()), orig_hash, "undo must restore exact bytes");
}

#[tokio::test]
async fn write_new_file_and_undo_removes_it() {
    let d = tempfile::tempdir().unwrap();
    let c = ctx_in(d.path(), PermissionMode::Yolo);
    execute(&c, &call("write_file", json!({"path": "src/new.rs", "content": "pub fn x() {}\n"}))).await.unwrap();
    assert!(d.path().join("src/new.rs").exists());
    c.undo.undo_last().unwrap();
    assert!(!d.path().join("src/new.rs").exists());
    assert!(execute(&c, &call("write_file", json!({"path": ".git/config", "content": "x"}))).await.is_err());
}

#[tokio::test]
async fn approval_flow_reject_leaves_file_untouched() {
    let d = tempfile::tempdir().unwrap();
    let f = d.path().join("a.txt");
    std::fs::write(&f, "hello\n").unwrap();
    let mut c = ctx_in(d.path(), PermissionMode::Ask);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    c.events = Some(tx);
    execute(&c, &call("read_file", json!({"path": "a.txt"}))).await.unwrap();
    let before = std::fs::read(&f).unwrap();
    let responder = tokio::spawn(async move {
        let mut diffs = Vec::new();
        while let Some(ev) = rx.recv().await {
            if let AgentEvent::ApprovalRequired(t) = ev {
                diffs.push(t.request.diff.clone().unwrap_or_default());
                t.respond(ApprovalDecision::Reject(Some("not now".into())));
            }
        }
        diffs
    });
    let e = execute(&c, &call("edit_file", json!({"path": "a.txt", "old_string": "hello", "new_string": "bye"}))).await.unwrap_err();
    assert!(e.to_string().contains("Rejected") && e.to_string().contains("not now"), "{e}");
    assert_eq!(std::fs::read(&f).unwrap(), before);
    drop(c);
    let diffs = responder.await.unwrap();
    assert!(diffs[0].contains("-hello") && diffs[0].contains("+bye"), "{:?}", diffs);
}

#[tokio::test]
async fn ask_mode_without_frontend_denies_bash_but_allows_allowlisted() {
    let d = tempfile::tempdir().unwrap();
    let c = ctx_in(d.path(), PermissionMode::Ask);
    let e = execute(&c, &call("bash", json!({"command": "touch x"}))).await.unwrap_err();
    assert!(e.to_string().contains("approval required"), "{e}");
    assert!(!d.path().join("x").exists());
    let ok = execute(&c, &call("bash", json!({"command": "echo hi"}))).await.unwrap();
    assert!(ok.contains("exit_code: 0") && ok.contains("hi"));
    // allowlisted command touching outside the workspace needs approval
    assert!(execute(&c, &call("bash", json!({"command": "cat /etc/hostname"}))).await.is_err());
}

#[tokio::test]
async fn bash_exit_code_truncation_timeout_and_key_scrub() {
    let d = tempfile::tempdir().unwrap();
    let c = ctx_in(d.path(), PermissionMode::Yolo);
    let out = execute(&c, &call("bash", json!({"command": "echo out; echo err >&2; exit 3"}))).await.unwrap();
    assert!(out.contains("exit_code: 3") && out.contains("--- stderr ---\nerr"), "{out}");
    let big = execute(&c, &call("bash", json!({"command": "head -c 3000000 /dev/zero | tr '\\0' a"}))).await.unwrap();
    assert!(big.len() < 32_000, "len {}", big.len());
    assert!(big.contains("truncated"));
    let t0 = Instant::now();
    let to = execute(&c, &call("bash", json!({"command": "sleep 30", "timeout_ms": 1000}))).await.unwrap();
    assert!(to.contains("killed: timeout"), "{to}");
    assert!(t0.elapsed() < Duration::from_secs(5));
    std::env::set_var("CACODE_TEST_FAKE_API_KEY", "sk-should-not-leak");
    let env = execute(&c, &call("bash", json!({"command": "env"}))).await.unwrap();
    assert!(!env.contains("sk-should-not-leak"));
    std::env::remove_var("CACODE_TEST_FAKE_API_KEY");
}

#[cfg(unix)]
#[tokio::test]
async fn cancel_kills_whole_process_group() {
    let d = tempfile::tempdir().unwrap();
    let c = ctx_in(d.path(), PermissionMode::Yolo);
    let marker = format!("cacode_cancel_probe_{}", std::process::id());
    let cmd = format!("bash -c 'exec -a {marker} sleep 300' & wait");
    let c2 = c.clone();
    let h = tokio::spawn(async move { execute(&c2, &call("bash", json!({"command": cmd}))).await });
    tokio::time::sleep(Duration::from_millis(500)).await;
    let alive = std::process::Command::new("pgrep").arg("-f").arg(&marker).output().unwrap();
    assert!(!alive.stdout.is_empty(), "probe should be running");
    c.cancel.cancel();
    let r = h.await.unwrap();
    assert!(r.unwrap_err().to_string().contains("Cancelled"));
    tokio::time::sleep(Duration::from_millis(300)).await;
    let alive = std::process::Command::new("pgrep").arg("-f").arg(&marker).output().unwrap();
    assert!(alive.stdout.is_empty(), "grandchild survived cancel: {}", String::from_utf8_lossy(&alive.stdout));
}

#[tokio::test]
async fn background_jobs() {
    let d = tempfile::tempdir().unwrap();
    let c = ctx_in(d.path(), PermissionMode::Yolo);
    let s = execute(&c, &call("bash", json!({"command": "for i in 1 2 3; do echo tick$i; sleep 0.2; done", "run_in_background": true}))).await.unwrap();
    assert!(s.contains("bash_1"));
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let o = execute(&c, &call("bash_output", json!({"job_id": "bash_1"}))).await.unwrap();
    assert!(o.contains("tick1") && o.contains("tick3") && o.contains("exit_code: 0"), "{o}");
    let o2 = execute(&c, &call("bash_output", json!({"job_id": "bash_1"}))).await.unwrap();
    assert!(!o2.contains("tick1"), "second read returns only new output");
}

#[tokio::test]
async fn grep_and_glob_respect_ignores() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path();
    for (f, body) in [
        ("src/main.rs", "fn main() {}\n"),
        ("target/debug/gen.rs", "fn main() {}\n"),
        ("node_modules/x/index.js", "function main() {}\n"),
        ("vendor/lib.rs", "fn main() {}\n"),
        ("secret/keys.rs", "fn main() {}\n"),
        ("notes.log", "fn main() {}\n"),
    ] {
        std::fs::create_dir_all(p.join(f).parent().unwrap()).unwrap();
        std::fs::write(p.join(f), body).unwrap();
    }
    std::fs::write(p.join(".gitignore"), "*.log\n").unwrap();
    std::fs::write(p.join(".ignore"), "vendor/\n").unwrap();
    std::fs::write(p.join(".cacodeignore"), "secret/\n").unwrap();
    let c = ctx_in(p, PermissionMode::Yolo);
    let g = execute(&c, &call("grep", json!({"pattern": "fn main"}))).await.unwrap();
    assert!(g.contains("src/main.rs:1:fn main() {}"), "{g}");
    for bad in ["target", "node_modules", "vendor", "secret", "notes.log"] {
        assert!(!g.contains(bad), "{bad} leaked into grep:\n{g}");
    }
    let gl = execute(&c, &call("glob", json!({"pattern": "**/*.rs"}))).await.unwrap();
    assert_eq!(gl.trim(), "src/main.rs");
    let legacy = execute(&c, &call("search_files", json!({"pattern": "fn main", "dir": "."}))).await.unwrap();
    assert!(legacy.starts_with("src/main.rs"));
}

#[tokio::test]
async fn find_symbol_tool() {
    let d = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(d.path().join("src")).unwrap();
    std::fs::write(d.path().join("src/lib.rs"), "pub struct ReActAgent;\nimpl ReActAgent {\n    pub fn run_turn(&self) {}\n}\n").unwrap();
    let c = ctx_in(d.path(), PermissionMode::Yolo);
    let out = execute(&c, &call("find_symbol", json!({"name": "ReActAgent"}))).await.unwrap();
    assert!(out.lines().next().unwrap().starts_with("src/lib.rs:1  struct ReActAgent"), "{out}");
}

#[tokio::test]
async fn path_lock_blocks_parallel_writers() {
    let d = tempfile::tempdir().unwrap();
    let c = ctx_in(d.path(), PermissionMode::Yolo);
    let p = d.path().join("f");
    let _l = c.lock_path(&p).unwrap();
    assert!(c.lock_path(&p).unwrap_err().to_string().contains("FileLocked"));
}

#[tokio::test]
async fn bwrap_sandbox_confines_writes_to_workspace() {
    if !bash::bwrap_available() {
        eprintln!("bwrap not available; skipping");
        return;
    }
    let d = tempfile::tempdir().unwrap();
    // outside dir under $HOME (the sandbox gives /tmp its own private tmpfs)
    let outside = tempfile::tempdir_in(dirs::home_dir().unwrap()).unwrap();
    let mut c = ctx_in(d.path(), PermissionMode::Yolo);
    c.sandbox = bash::SandboxConfig { mode: bash::SandboxMode::Required, writable: vec![], network: true };
    let inside = execute(&c, &call("bash", json!({"command": "echo hi > inside.txt && cat inside.txt"}))).await.unwrap();
    assert!(inside.contains("exit_code: 0") && inside.contains("hi"), "{inside}");
    assert!(d.path().join("inside.txt").exists());
    let target = outside.path().join("escape.txt");
    let out = execute(&c, &call("bash", json!({"command": format!("echo x > {}", target.display())}))).await.unwrap();
    assert!(!out.contains("exit_code: 0"), "write outside must fail: {out}");
    assert!(out.contains("[sandbox]"), "{out}");
    assert!(!target.exists());
    // reading outside is still possible (read-only bind)
    let r = execute(&c, &call("bash", json!({"command": "test -r /etc/hostname && echo readable"}))).await.unwrap();
    assert!(r.contains("readable"));
    // network can be cut
    c.sandbox.network = false;
    let n = execute(&c, &call("bash", json!({"command": "cat /proc/net/dev | wc -l"}))).await.unwrap();
    assert!(n.contains("exit_code: 0"));
    // off → unwrapped
    c.sandbox.mode = bash::SandboxMode::Off;
    let o = execute(&c, &call("bash", json!({"command": format!("echo x > {}", target.display())}))).await.unwrap();
    assert!(o.contains("exit_code: 0") && target.exists());
}

#[tokio::test]
async fn todo_write_validates_and_emits() {
    let d = tempfile::tempdir().unwrap();
    let mut c = ctx_in(d.path(), PermissionMode::Ask);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    c.events = Some(tx);
    let out = execute(&c, &call("todo_write", json!({"todos": [{"content": "read code", "status": "completed"}, {"content": "fix bug", "status": "in_progress"}, {"content": "run tests", "status": "pending"}]}))).await.unwrap();
    assert!(out.contains("1/3 completed") && out.contains("[>] fix bug"), "{out}");
    match rx.try_recv().unwrap() {
        AgentEvent::Todos(t) => assert_eq!(t.len(), 3),
        e => panic!("unexpected {e:?}"),
    }
    assert!(execute(&c, &call("todo_write", json!({"todos": [{"content": "x", "status": "doing"}]}))).await.is_err());
}

#[tokio::test]
async fn plan_mode_blocks_edits_until_plan_approved() {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("a.txt"), "hello\n").unwrap();
    let mut c = ctx_in(d.path(), PermissionMode::Plan);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    c.events = Some(tx);
    assert!(!specs_for(&c).is_empty() && specs_for(&c).iter().any(|s| s.name == "exit_plan_mode"));
    execute(&c, &call("read_file", json!({"path": "a.txt"}))).await.unwrap();
    let e = execute(&c, &call("edit_file", json!({"path": "a.txt", "old_string": "hello", "new_string": "bye"}))).await.unwrap_err();
    assert!(e.to_string().contains("plan mode"), "{e}");
    assert!(execute(&c, &call("bash", json!({"command": "git status"}))).await.is_ok(), "allowlisted read commands still run");
    let c2 = c.clone();
    let h = tokio::spawn(async move { execute(&c2, &call("exit_plan_mode", json!({"plan": "1. change hello to bye"}))).await });
    let mut changed = None;
    while let Some(ev) = rx.recv().await {
        match ev {
            AgentEvent::ApprovalRequired(t) => {
                assert_eq!(t.request.diff.as_deref(), Some("1. change hello to bye"));
                t.respond(ApprovalDecision::ApproveAlways);
            }
            AgentEvent::ModeChanged(m) => {
                changed = Some(m);
                break;
            }
            _ => {}
        }
    }
    assert!(h.await.unwrap().unwrap().contains("Plan approved"));
    assert_eq!(changed, Some(PermissionMode::AutoEdit));
    assert_eq!(c.mode(), PermissionMode::AutoEdit);
    assert!(!specs_for(&c).iter().any(|s| s.name == "exit_plan_mode"));
    execute(&c, &call("edit_file", json!({"path": "a.txt", "old_string": "hello", "new_string": "bye"}))).await.unwrap();
    assert_eq!(std::fs::read_to_string(d.path().join("a.txt")).unwrap(), "bye\n");
}

#[tokio::test]
async fn web_fetch_converts_html_and_gates_domains() {
    let srv = crate::testutil::MockServer::start(vec![crate::testutil::MockResp {
        status: 200,
        headers: vec![("content-type".into(), "text/html; charset=utf-8".into())],
        body: "<html><head><style>.x{}</style><script>evil()</script></head><body><h1>Tokio Docs</h1><p>Use <code>spawn</code> to run tasks.</p></body></html>".into(),
        delay: std::time::Duration::ZERO,
    }])
    .await;
    let d = tempfile::tempdir().unwrap();
    let mut c = ctx_in(d.path(), PermissionMode::Ask);
    let url = format!("{}/docs", srv.url());
    // 127.0.0.1 is not allowlisted and there is no front-end → denied
    let e = execute(&c, &call("web_fetch", json!({"url": url}))).await.unwrap_err();
    assert!(e.to_string().contains("Rejected fetch of 127.0.0.1"), "{e}");
    c.web_allowlist.push("127.0.0.1".into());
    let out = execute(&c, &call("web_fetch", json!({"url": url}))).await.unwrap();
    assert!(out.starts_with("[200 "), "{out}");
    assert!(out.contains("Tokio Docs") && out.contains("spawn"), "{out}");
    assert!(!out.contains("evil()") && !out.contains("<p>"), "{out}");
    assert!(execute(&c, &call("web_fetch", json!({"url": "file:///etc/passwd"}))).await.is_err());
    assert!(web::domain_allowed("api.docs.rs", &["docs.rs".into()]));
    assert!(!web::domain_allowed("docs.rs.evil.com", &["docs.rs".into()]));
}
