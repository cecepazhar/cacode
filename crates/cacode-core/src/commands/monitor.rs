//! Local token-usage proxy (`cacode monitor`, default 127.0.0.1:8000).
//!
//! * Streams SSE responses chunk-by-chunk (no buffering → no added TTFT) while
//!   teeing them into a parser that extracts token usage at the end.
//! * Injects `stream_options.include_usage` into OpenAI-style streaming
//!   requests so usage is reported exactly instead of estimated.
//! * Strips hop-by-hop headers, rate-limits with `Retry-After`, and fails
//!   over to `--fallback` endpoints on connect errors / 429 / 5xx.

use crate::llm::sse::{drain_events, StreamAccumulator};
use crate::llm::Wire;
use crate::pricing::Pricing;
use anyhow::Result;
use axum::extract::Extension;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, Response, StatusCode, Uri};
use axum::routing::any;
use axum::Router;
use bytes::Bytes;
use futures_util::StreamExt;
use reqwest::Client;
use rusqlite::Connection;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const HOP_BY_HOP: &[&str] = &[
    "connection", "keep-alive", "proxy-authenticate", "proxy-authorization", "te", "trailer", "trailers", "transfer-encoding", "upgrade", "host", "content-length", "accept-encoding",
];

fn load_pricing(costs_path: &Option<String>) -> HashMap<String, Pricing> {
    if let Some(path) = costs_path {
        match std::fs::read_to_string(path).map(|c| toml::from_str::<HashMap<String, Pricing>>(&c)) {
            Ok(Ok(table)) => {
                eprintln!("[monitor] Loaded pricing from {}", path);
                return table;
            }
            Ok(Err(e)) => eprintln!("[monitor] Warning: failed to parse pricing file {}: {}", path, e),
            Err(e) => eprintln!("[monitor] Warning: could not read pricing file {}: {}", path, e),
        }
    }
    HashMap::new() // built-in table + ~/.cacode/pricing.toml are used by crate::pricing
}

/// Sliding-window rate limiter returning the wait time when saturated.
struct RateLimiter {
    timestamps: Mutex<Vec<Instant>>,
    max_rpm: usize,
}

impl RateLimiter {
    fn new(max_rpm: usize) -> Self {
        Self { timestamps: Mutex::new(Vec::new()), max_rpm: max_rpm.max(1) }
    }
    fn check(&self) -> Result<(), Duration> {
        let mut ts = self.timestamps.lock().unwrap_or_else(|p| p.into_inner());
        let now = Instant::now();
        let window = Duration::from_secs(60);
        ts.retain(|t| now.duration_since(*t) < window);
        if ts.len() >= self.max_rpm {
            Err(window.saturating_sub(now.duration_since(ts[0])))
        } else {
            ts.push(now);
            Ok(())
        }
    }
}

struct AppState {
    endpoints: Vec<String>,
    client: Client,
    db: Arc<Mutex<Connection>>,
    pricing: HashMap<String, Pricing>,
    rate_limiter: RateLimiter,
}

fn record(state: &AppState, endpoint: &str, model: &str, usage: &crate::types::TokenUsage) {
    if usage.total_tokens == 0 {
        return;
    }
    let provider = reqwest::Url::parse(endpoint).ok().and_then(|u| u.host_str().map(String::from)).unwrap_or_default();
    let cost = crate::pricing::lookup_in(model, &state.pricing)
        .map(|p| (usage.prompt_tokens as f64 * p.input_per_million + usage.completion_tokens as f64 * p.output_per_million) / 1_000_000.0);
    if cost.is_none() {
        eprintln!("[monitor] Warning: no pricing for model '{}' — recorded with cost_known=0", model);
    }
    let db = state.db.clone();
    let (model, u) = (model.to_string(), usage.clone());
    tokio::task::spawn_blocking(move || {
        if let Ok(conn) = db.lock() {
            let r = conn.execute(
                "INSERT INTO token_usage (timestamp, model, prompt_tokens, completion_tokens, total_tokens, est_cost, provider, source, estimated, cost_known)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'proxy', ?8, ?9)",
                rusqlite::params![chrono::Utc::now().to_rfc3339(), model, u.prompt_tokens, u.completion_tokens, u.total_tokens, cost.unwrap_or(0.0), provider, u.estimated as i64, cost.is_some() as i64],
            );
            if let Err(e) = r {
                eprintln!("[monitor] DB write error: {}", e);
            }
        }
    });
}

fn wire_for(path: &str) -> Wire {
    if path.contains("/messages") { Wire::Anthropic } else { Wire::OpenAi }
}

/// Ask OpenAI-compatible servers to include usage in the final stream chunk.
fn inject_include_usage(path: &str, body: &Bytes) -> Bytes {
    if !path.contains("chat/completions") {
        return body.clone();
    }
    let Ok(mut v) = serde_json::from_slice::<serde_json::Value>(body) else { return body.clone() };
    if v.get("stream").and_then(|s| s.as_bool()) == Some(true) && v.get("stream_options").is_none() {
        v["stream_options"] = serde_json::json!({"include_usage": true});
        return Bytes::from(serde_json::to_vec(&v).unwrap_or_else(|_| body.to_vec()));
    }
    body.clone()
}

async fn proxy_handler(method: Method, uri: Uri, headers: HeaderMap, Extension(state): Extension<Arc<AppState>>, body: Bytes) -> Response<axum::body::Body> {
    if let Err(wait) = state.rate_limiter.check() {
        return Response::builder()
            .status(StatusCode::TOO_MANY_REQUESTS)
            .header("retry-after", wait.as_secs().max(1).to_string())
            .body(axum::body::Body::from("{\"error\":{\"message\":\"cacode monitor: local rate limit reached\",\"type\":\"rate_limit_error\"}}"))
            .unwrap_or_default();
    }
    let path_q = uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("").to_string();
    let wire = wire_for(uri.path());
    let body = inject_include_usage(uri.path(), &body);
    let req_model = serde_json::from_slice::<serde_json::Value>(&body).ok().and_then(|v| v.get("model").and_then(|m| m.as_str()).map(String::from));

    let mut last_status = StatusCode::BAD_GATEWAY;
    let mut last_body = String::from("{\"error\":{\"message\":\"cacode monitor: all upstream endpoints failed\"}}");
    for (i, endpoint) in state.endpoints.iter().enumerate() {
        let is_last = i + 1 == state.endpoints.len();
        let target_url = format!("{}{}", endpoint.trim_end_matches('/'), path_q);
        let mut rb = state.client.request(method.clone(), &target_url);
        for (name, value) in headers.iter() {
            if !HOP_BY_HOP.contains(&name.as_str()) {
                rb = rb.header(name, value);
            }
        }
        let resp = match rb.body(body.clone()).send().await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[monitor] upstream {} error: {}{}", endpoint, e, if is_last { "" } else { " — failing over" });
                last_body = format!("{{\"error\":{{\"message\":\"upstream unreachable: {}\"}}}}", e.to_string().replace('"', "'"));
                continue;
            }
        };
        let status = resp.status();
        if !is_last && (status.as_u16() == 429 || status.is_server_error()) {
            eprintln!("[monitor] upstream {} returned {} — failing over", endpoint, status);
            last_status = StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            last_body = resp.text().await.unwrap_or_default();
            continue;
        }
        let mut builder = Response::builder().status(status.as_u16());
        let is_sse = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).map(|v| v.contains("event-stream")).unwrap_or(false);
        for (name, value) in resp.headers().iter() {
            if !HOP_BY_HOP.contains(&name.as_str()) {
                if let (Ok(n), Ok(v)) = (HeaderName::from_bytes(name.as_str().as_bytes()), HeaderValue::from_bytes(value.as_bytes())) {
                    builder = builder.header(n, v);
                }
            }
        }

        if is_sse {
            // Tee: forward every chunk immediately, parse a copy for usage.
            struct Tee {
                upstream: futures_util::stream::BoxStream<'static, reqwest::Result<Bytes>>,
                acc: Option<StreamAccumulator>,
                buf: String,
                state: Arc<AppState>,
                endpoint: String,
                req_model: Option<String>,
            }
            let tee = Tee { upstream: resp.bytes_stream().boxed(), acc: Some(StreamAccumulator::new(wire)), buf: String::new(), state: state.clone(), endpoint: endpoint.clone(), req_model: req_model.clone() };
            let stream = futures_util::stream::unfold(tee, |mut t| async move {
                match t.upstream.next().await {
                    Some(Ok(chunk)) => {
                        t.buf.push_str(&String::from_utf8_lossy(&chunk));
                        if let Some(acc) = t.acc.as_mut() {
                            for d in drain_events(&mut t.buf) {
                                acc.feed(&d);
                            }
                        }
                        Some((Ok::<Bytes, std::io::Error>(chunk), t))
                    }
                    Some(Err(e)) => Some((Err(std::io::Error::other(e)), t)),
                    None => {
                        if let Some(mut acc) = t.acc.take() {
                            t.buf.push_str("\n\n");
                            for d in drain_events(&mut t.buf) {
                                acc.feed(&d);
                            }
                            let model = acc.model.clone().or(t.req_model.clone()).unwrap_or_else(|| "unknown".into());
                            let mut usage = acc.usage.clone();
                            if usage.total_tokens == 0 {
                                usage.completion_tokens = crate::text::estimate_tokens(&acc.content);
                                usage.total_tokens = usage.completion_tokens;
                                usage.estimated = true;
                            }
                            record(&t.state, &t.endpoint, &model, &usage);
                        }
                        None
                    }
                }
            });
            return builder.body(axum::body::Body::from_stream(stream)).unwrap_or_default();
        }

        let resp_bytes = match resp.bytes().await {
            Ok(b) => b,
            Err(e) => {
                last_body = format!("{{\"error\":{{\"message\":\"upstream body error: {}\"}}}}", e.to_string().replace('"', "'"));
                continue;
            }
        };
        if let Ok(parsed) = crate::llm::sse::parse_full_body(&String::from_utf8_lossy(&resp_bytes), wire) {
            let model = serde_json::from_slice::<serde_json::Value>(&resp_bytes)
                .ok()
                .and_then(|v| v.get("model").and_then(|m| m.as_str()).map(String::from))
                .or(req_model.clone())
                .unwrap_or_else(|| "unknown".into());
            record(&state, endpoint, &model, &parsed.usage);
        }
        return builder.body(axum::body::Body::from(resp_bytes)).unwrap_or_default();
    }
    Response::builder().status(last_status).header("content-type", "application/json").body(axum::body::Body::from(last_body)).unwrap_or_default()
}

pub fn router(endpoints: Vec<String>, pricing: HashMap<String, Pricing>, max_rpm: usize, conn: Connection) -> Router {
    router_with_db(endpoints, pricing, max_rpm, Arc::new(Mutex::new(conn)))
}

pub fn router_with_db(endpoints: Vec<String>, pricing: HashMap<String, Pricing>, max_rpm: usize, db: Arc<Mutex<Connection>>) -> Router {
    let state = Arc::new(AppState {
        endpoints,
        client: Client::builder().connect_timeout(Duration::from_secs(10)).build().unwrap_or_default(),
        db,
        pricing,
        rate_limiter: RateLimiter::new(max_rpm),
    });
    Router::new().fallback(any(proxy_handler)).layer(Extension(state))
}

pub async fn run(endpoint: String, costs: Option<String>, port: Option<u16>, max_rpm: Option<usize>) -> Result<()> {
    run_with_fallbacks(endpoint, Vec::new(), costs, port, max_rpm).await
}

pub async fn run_with_fallbacks(endpoint: String, fallbacks: Vec<String>, costs: Option<String>, port: Option<u16>, max_rpm: Option<usize>) -> Result<()> {
    let conn = crate::usage_db::open().ok_or_else(|| anyhow::anyhow!("cannot open ~/.cacode/usage.db"))?;
    let pricing = load_pricing(&costs);
    let listen_port = port.unwrap_or(8000);
    let rpm = max_rpm.unwrap_or(120);
    let mut endpoints = vec![endpoint.clone()];
    endpoints.extend(fallbacks.into_iter().filter(|f| !f.trim().is_empty()));
    let app = router(endpoints.clone(), pricing, rpm, conn);
    let addr: SocketAddr = format!("127.0.0.1:{}", listen_port).parse().map_err(|e| anyhow::anyhow!("Invalid address: {}", e))?;
    println!("[monitor] Starting proxy at {} forwarding to {} (max {} RPM)", addr, endpoints.join(" → "), rpm);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).with_graceful_shutdown(shutdown_signal()).await.map_err(|e| anyhow::anyhow!(e))?;
    println!("[monitor] Shutdown complete.");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sigterm) => {
                tokio::select! {
                    _ = ctrl_c => { eprintln!("\n[monitor] Received Ctrl+C, shutting down..."); }
                    _ = sigterm.recv() => { eprintln!("\n[monitor] Received SIGTERM, shutting down..."); }
                }
            }
            Err(_) => {
                let _ = ctrl_c.await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = ctrl_c.await;
        eprintln!("\n[monitor] Received Ctrl+C, shutting down...");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Upstream that streams 3 SSE events 400 ms apart (chunked encoding).
    async fn slow_sse_upstream(seen_body: Arc<Mutex<String>>) -> String {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                let seen = seen_body.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 65536];
                    let mut n = 0;
                    loop {
                        let k = s.read(&mut buf[n..]).await.unwrap_or(0);
                        n += k;
                        let t = String::from_utf8_lossy(&buf[..n]).to_string();
                        if let Some(h) = t.find("\r\n\r\n") {
                            let cl = t.lines().find(|l| l.to_lowercase().starts_with("content-length:")).and_then(|l| l[15..].trim().parse::<usize>().ok()).unwrap_or(0);
                            if n >= h + 4 + cl {
                                *seen.lock().unwrap() = t[h + 4..].to_string();
                                break;
                            }
                        }
                        if k == 0 {
                            return;
                        }
                    }
                    let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").await;
                    let events = [
                        r#"data: {"model":"gpt-4o","choices":[{"delta":{"content":"Hel"}}]}"#,
                        r#"data: {"choices":[{"delta":{"content":"lo"}}]}"#,
                        r#"data: {"choices":[],"usage":{"prompt_tokens":11,"completion_tokens":2,"total_tokens":13}}"#,
                    ];
                    for e in events {
                        let chunk = format!("{}\n\n", e);
                        let _ = s.write_all(format!("{:x}\r\n{}\r\n", chunk.len(), chunk).as_bytes()).await;
                        let _ = s.flush().await;
                        tokio::time::sleep(Duration::from_millis(400)).await;
                    }
                    let _ = s.write_all(b"0\r\n\r\n").await;
                });
            }
        });
        format!("http://127.0.0.1:{}", port)
    }

    #[tokio::test]
    async fn sse_is_streamed_through_and_usage_recorded() {
        let seen = Arc::new(Mutex::new(String::new()));
        let upstream = slow_sse_upstream(seen.clone()).await;
        let upstream_url = upstream.clone();
        let conn = Connection::open_in_memory().unwrap();
        crate::usage_db::init(&conn).unwrap();
        let db = Arc::new(Mutex::new(conn));
        let app = router_with_db(vec![upstream], HashMap::new(), 100, db.clone());
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });

        // baseline: direct to upstream
        let td = Instant::now();
        let direct = reqwest::Client::new().post(format!("{}/v1/chat/completions", upstream_url)).json(&serde_json::json!({"model": "gpt-4o", "stream": true})).send().await.unwrap();
        let mut ds = direct.bytes_stream();
        let _ = ds.next().await;
        let direct_ttft = td.elapsed();
        drop(ds);

        let t0 = Instant::now();
        let resp = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{}/v1/chat/completions", port))
            .json(&serde_json::json!({"model": "gpt-4o", "stream": true, "messages": []}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert!(resp.headers().get("transfer-encoding").map(|v| v != "chunked, chunked").unwrap_or(true));
        let mut stream = resp.bytes_stream();
        let first = stream.next().await.unwrap().unwrap();
        let ttft = t0.elapsed();
        assert!(String::from_utf8_lossy(&first).contains("Hel"));
        assert!(ttft < Duration::from_millis(350), "first chunk took {ttft:?} — proxy is buffering");
        let overhead = ttft.saturating_sub(direct_ttft);
        assert!(overhead < Duration::from_millis(50), "proxy TTFT overhead {overhead:?} (direct {direct_ttft:?}, proxy {ttft:?})");
        let mut rest = String::new();
        while let Some(c) = stream.next().await {
            rest.push_str(&String::from_utf8_lossy(&c.unwrap()));
        }
        let total = t0.elapsed();
        assert!(total >= Duration::from_millis(800));
        assert!(rest.contains("usage"));
        // include_usage was injected into the upstream request
        let fwd: serde_json::Value = serde_json::from_str(&seen.lock().unwrap()).unwrap();
        assert_eq!(fwd["stream_options"]["include_usage"], true);
        tokio::time::sleep(Duration::from_millis(200)).await;
        let (model, p, c, cost, src): (String, i64, i64, f64, String) = db
            .lock()
            .unwrap()
            .query_row("SELECT model, prompt_tokens, completion_tokens, est_cost, source FROM token_usage", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
            .unwrap();
        assert_eq!((model.as_str(), p, c, src.as_str()), ("gpt-4o", 11, 2, "proxy"));
        assert!((cost - (11.0 * 2.5 + 2.0 * 10.0) / 1e6).abs() < 1e-12);
        println!("TTFT direct: {:?}, via proxy: {:?} (overhead {:?}), total stream: {:?}", direct_ttft, ttft, overhead, total);
    }

    #[tokio::test]
    async fn failover_and_local_rate_limit() {
        let bad = crate::testutil::MockServer::start(vec![crate::testutil::MockResp::status(503, "down")]).await;
        let good = crate::testutil::MockServer::start(vec![crate::testutil::MockResp::openai_text("ok")]).await;
        let conn = Connection::open_in_memory().unwrap();
        crate::usage_db::init(&conn).unwrap();
        let app = router(vec![bad.url(), good.url()], HashMap::new(), 2, conn);
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        let url = format!("http://127.0.0.1:{}/v1/chat/completions", port);
        let c = reqwest::Client::new();
        let r = c.post(&url).json(&serde_json::json!({"model": "gpt-4o"})).send().await.unwrap();
        assert_eq!(r.status(), 200);
        assert!(r.text().await.unwrap().contains("ok"));
        assert_eq!(bad.count(), 1);
        let _ = c.post(&url).json(&serde_json::json!({})).send().await.unwrap();
        let limited = c.post(&url).json(&serde_json::json!({})).send().await.unwrap();
        assert_eq!(limited.status(), 429);
        assert!(limited.headers().get("retry-after").is_some());
    }
}
