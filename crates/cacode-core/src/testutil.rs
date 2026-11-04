//! Scriptable mock HTTP server for provider / agent tests (test-only).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Clone, Debug)]
pub struct MockResp {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
    pub delay: Duration,
}

impl MockResp {
    pub fn json(body: serde_json::Value) -> Self {
        Self { status: 200, headers: vec![("content-type".into(), "application/json".into())], body: body.to_string(), delay: Duration::ZERO }
    }
    pub fn status(status: u16, body: &str) -> Self {
        Self { status, headers: vec![("content-type".into(), "application/json".into())], body: body.into(), delay: Duration::ZERO }
    }
    pub fn sse(events: &[serde_json::Value]) -> Self {
        let mut body = String::new();
        for e in events {
            body.push_str(&format!("data: {}\n\n", e));
        }
        body.push_str("data: [DONE]\n\n");
        Self { status: 200, headers: vec![("content-type".into(), "text/event-stream".into())], body, delay: Duration::ZERO }
    }
    pub fn header(mut self, k: &str, v: &str) -> Self {
        self.headers.push((k.into(), v.into()));
        self
    }
    pub fn delayed(mut self, d: Duration) -> Self {
        self.delay = d;
        self
    }
    /// OpenAI chat completion with plain text.
    pub fn openai_text(text: &str) -> Self {
        Self::json(serde_json::json!({"model": "mock", "choices": [{"message": {"role": "assistant", "content": text}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}}))
    }
    /// OpenAI chat completion with native tool calls `(name, args)`.
    pub fn openai_tools(calls: &[(&str, serde_json::Value)]) -> Self {
        let tc: Vec<serde_json::Value> = calls
            .iter()
            .enumerate()
            .map(|(i, (n, a))| serde_json::json!({"id": format!("call_{}", i), "type": "function", "function": {"name": n, "arguments": a.to_string()}}))
            .collect();
        Self::json(serde_json::json!({"model": "mock", "choices": [{"message": {"role": "assistant", "content": null, "tool_calls": tc}, "finish_reason": "tool_calls"}], "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}}))
    }
}

#[derive(Clone, Debug)]
pub struct Recorded {
    pub path: String,
    pub body: String,
    pub at: Instant,
}

pub type Handler = Arc<dyn Fn(&serde_json::Value) -> MockResp + Send + Sync>;

pub struct MockServer {
    pub port: u16,
    pub requests: Arc<Mutex<Vec<Recorded>>>,
}

impl MockServer {
    /// Serve `script` in order; after it is exhausted repeat the last entry.
    pub async fn start(script: Vec<MockResp>) -> Self {
        let script = Arc::new(script);
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        Self::start_fn(Arc::new(move |_b: &serde_json::Value| {
            let idx = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            script.get(idx).or_else(|| script.last()).cloned().expect("non-empty script")
        }))
        .await
    }

    /// Respond based on the request body (for concurrent / content-dependent tests).
    pub async fn start_fn(handler: Handler) -> Self {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = l.local_addr().expect("addr").port();
        let requests: Arc<Mutex<Vec<Recorded>>> = Arc::default();
        let reqs = requests.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = l.accept().await else { break };
                let reqs = reqs.clone();
                let handler = handler.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4 << 20];
                    let mut n = 0;
                    let (path, body) = loop {
                        let Ok(k) = s.read(&mut buf[n..]).await else { return };
                        n += k;
                        let t = String::from_utf8_lossy(&buf[..n]).to_string();
                        if let Some(h) = t.find("\r\n\r\n") {
                            let cl = t
                                .lines()
                                .find(|l| l.to_lowercase().starts_with("content-length:"))
                                .and_then(|l| l[15..].trim().parse::<usize>().ok())
                                .unwrap_or(0);
                            if n >= h + 4 + cl {
                                let path = t.split_whitespace().nth(1).unwrap_or("/").to_string();
                                break (path, t[h + 4..h + 4 + cl.min(t.len() - h - 4)].to_string());
                            }
                        }
                        if k == 0 {
                            return;
                        }
                    };
                    let json: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
                    reqs.lock().unwrap().push(Recorded { path, body, at: Instant::now() });
                    let r = handler(&json);
                    if !r.delay.is_zero() {
                        tokio::time::sleep(r.delay).await;
                    }
                    let mut head = format!("HTTP/1.1 {} X\r\nContent-Length: {}\r\nConnection: close\r\n", r.status, r.body.len());
                    for (k, v) in &r.headers {
                        head.push_str(&format!("{}: {}\r\n", k, v));
                    }
                    head.push_str("\r\n");
                    let _ = s.write_all(head.as_bytes()).await;
                    let _ = s.write_all(r.body.as_bytes()).await;
                    let _ = s.shutdown().await;
                });
            }
        });
        Self { port, requests }
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub fn bodies(&self) -> Vec<serde_json::Value> {
        self.requests.lock().unwrap().iter().filter_map(|r| serde_json::from_str(&r.body).ok()).collect()
    }

    pub fn count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}
