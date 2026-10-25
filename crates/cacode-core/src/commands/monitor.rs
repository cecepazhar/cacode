use anyhow::Result;
use axum::Router;
use axum::extract::Extension;
use axum::http::{HeaderMap, Method, Uri, Response, StatusCode};
use axum::routing::any;
use hyper::body::Bytes;
use reqwest::Client;
use rusqlite::{params, Connection};
use serde::Deserialize;
use serde_json::Value;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use chrono::Utc;
use std::collections::HashMap;
use dirs;

#[derive(Debug, Clone, Deserialize)]
struct Pricing {
    input_per_million: f64,
    output_per_million: f64,
}

fn default_pricing() -> HashMap<String, Pricing> {
    let mut m = HashMap::new();
    // OpenAI
    m.insert("gpt-4o".into(), Pricing { input_per_million: 2.50, output_per_million: 10.0 });
    m.insert("gpt-4o-mini".into(), Pricing { input_per_million: 0.15, output_per_million: 0.60 });
    m.insert("gpt-4.1".into(), Pricing { input_per_million: 2.00, output_per_million: 8.00 });
    m.insert("gpt-4.1-mini".into(), Pricing { input_per_million: 0.40, output_per_million: 1.60 });
    m.insert("gpt-4.1-nano".into(), Pricing { input_per_million: 0.10, output_per_million: 0.40 });
    m.insert("o3".into(), Pricing { input_per_million: 2.00, output_per_million: 8.00 });
    m.insert("o4-mini".into(), Pricing { input_per_million: 1.10, output_per_million: 4.40 });
    // Anthropic
    m.insert("claude-4-opus".into(), Pricing { input_per_million: 15.0, output_per_million: 75.0 });
    m.insert("claude-4-sonnet".into(), Pricing { input_per_million: 3.0, output_per_million: 15.0 });
    m.insert("claude-3.5-sonnet".into(), Pricing { input_per_million: 3.0, output_per_million: 15.0 });
    m.insert("claude-3-sonnet-20240229".into(), Pricing { input_per_million: 3.0, output_per_million: 15.0 });
    m
}

fn load_pricing(costs_path: &Option<String>) -> HashMap<String, Pricing> {
    // Try explicit path first
    if let Some(path) = costs_path {
        if let Ok(content) = std::fs::read_to_string(path) {
            if let Ok(table) = toml::from_str::<HashMap<String, Pricing>>(&content) {
                eprintln!("[monitor] Loaded pricing from {}", path);
                return table;
            } else {
                eprintln!("[monitor] Warning: failed to parse pricing file {}", path);
            }
        } else {
            eprintln!("[monitor] Warning: could not read pricing file {}", path);
        }
    }
    // Try ~/.cacode/pricing.toml
    if let Some(home) = dirs::home_dir() {
        let default_path = home.join(".cacode").join("pricing.toml");
        if default_path.exists() {
            if let Ok(content) = std::fs::read_to_string(&default_path) {
                if let Ok(table) = toml::from_str::<HashMap<String, Pricing>>(&content) {
                    eprintln!("[monitor] Loaded pricing from {}", default_path.display());
                    return table;
                }
            }
        }
    }
    default_pricing()
}

// Simple sliding-window rate limiter
struct RateLimiter {
    timestamps: Mutex<Vec<Instant>>,
    max_rpm: usize,
}

impl RateLimiter {
    fn new(max_rpm: usize) -> Self {
        Self {
            timestamps: Mutex::new(Vec::new()),
            max_rpm,
        }
    }

    fn check(&self) -> bool {
        let mut ts = self.timestamps.lock().unwrap();
        let now = Instant::now();
        let cutoff = now - std::time::Duration::from_secs(60);
        ts.retain(|t| *t > cutoff);
        if ts.len() >= self.max_rpm {
            false
        } else {
            ts.push(now);
            true
        }
    }
}

#[derive(Clone)]
struct AppState {
    real_endpoint: String,
    client: Client,
    db: Arc<Mutex<Connection>>,
    pricing: Arc<HashMap<String, Pricing>>,
    rate_limiter: Arc<RateLimiter>,
}

async fn proxy_handler(
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    Extension(state): Extension<Arc<AppState>>,
    body: Bytes,
) -> Result<Response<axum::body::Body>, StatusCode> {
    // Rate limit check
    if !state.rate_limiter.check() {
        return Err(StatusCode::TOO_MANY_REQUESTS);
    }

    let target_url = format!(
        "{}{}",
        state.real_endpoint.trim_end_matches('/'),
        uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("")
    );
    let client = &state.client;
    let mut req_builder = client.request(method.clone(), &target_url);
    for (name, value) in headers.iter() {
        if name != "host" {
            req_builder = req_builder.header(name, value);
        }
    }
    req_builder = req_builder.body(body.clone());
    let resp = req_builder.send().await.map_err(|_| StatusCode::BAD_GATEWAY)?;
    let status = resp.status();
    let resp_headers = resp.headers().clone();
    let resp_bytes = resp.bytes().await.map_err(|_| StatusCode::BAD_GATEWAY)?;

    // Parse JSON body for usage — do NOT store raw_response
    if let Ok(json) = serde_json::from_slice::<Value>(&resp_bytes) {
        let model = json.get("model").and_then(|v| v.as_str()).unwrap_or("unknown");
        let (prompt, completion, total) = if let Some(usage) = json.get("usage") {
            (
                usage.get("prompt_tokens").or_else(|| usage.get("input_tokens")).and_then(|v| v.as_i64()).unwrap_or(0),
                usage.get("completion_tokens").or_else(|| usage.get("output_tokens")).and_then(|v| v.as_i64()).unwrap_or(0),
                usage.get("total_tokens").and_then(|v| v.as_i64()).unwrap_or_else(|| {
                    let p = usage.get("prompt_tokens").or_else(|| usage.get("input_tokens")).and_then(|v| v.as_i64()).unwrap_or(0);
                    let c = usage.get("completion_tokens").or_else(|| usage.get("output_tokens")).and_then(|v| v.as_i64()).unwrap_or(0);
                    p + c
                }),
            )
        } else {
            (0, 0, 0)
        };
        if total > 0 {
            let pricing = match state.pricing.get(model) {
                Some(p) => p.clone(),
                None => {
                    eprintln!("[monitor] Warning: unknown model '{}', cost estimated as $0", model);
                    Pricing { input_per_million: 0.0, output_per_million: 0.0 }
                }
            };
            let est_cost = (prompt as f64 * pricing.input_per_million + completion as f64 * pricing.output_per_million) / 1_000_000.0;
            let ts = Utc::now().to_rfc3339();
            let model_str = model.to_string();
            let db = state.db.clone();
            tokio::task::spawn_blocking(move || {
                if let Ok(conn) = db.lock() {
                    if let Err(e) = conn.execute(
                        "INSERT INTO token_usage (timestamp, model, prompt_tokens, completion_tokens, total_tokens, est_cost) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                        params![ts, model_str, prompt, completion, total, est_cost],
                    ) {
                        eprintln!("[monitor] DB write error: {}", e);
                    }
                }
            });
        }
    }

    let mut builder = Response::builder().status(status);
    for (name, value) in resp_headers.iter() {
        builder = builder.header(name, value);
    }
    let body = axum::body::Body::from(resp_bytes);
    let response = builder.body(body).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(response)
}

pub async fn run(endpoint: String, costs: Option<String>, port: Option<u16>, max_rpm: Option<usize>) -> Result<()> {
    let db_path = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Home directory not found"))?
        .join(".cacode")
        .join("usage.db");
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let conn = Connection::open(&db_path)?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS token_usage (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp TEXT NOT NULL,
            model TEXT NOT NULL,
            prompt_tokens INTEGER NOT NULL,
            completion_tokens INTEGER NOT NULL,
            total_tokens INTEGER NOT NULL,
            est_cost REAL NOT NULL
        )",
        [],
    )?;

    let pricing = load_pricing(&costs);
    let listen_port = port.unwrap_or(8000);
    let rpm = max_rpm.unwrap_or(120);

    let state = Arc::new(AppState {
        real_endpoint: endpoint.clone(),
        client: Client::new(),
        db: Arc::new(Mutex::new(conn)),
        pricing: Arc::new(pricing),
        rate_limiter: Arc::new(RateLimiter::new(rpm)),
    });

    let app = Router::new()
        .fallback(any(proxy_handler))
        .layer(Extension(state));

    let addr: SocketAddr = format!("127.0.0.1:{}", listen_port)
        .parse()
        .map_err(|e| anyhow::anyhow!("Invalid address: {}", e))?;
    println!("[monitor] Starting proxy at {} forwarding to {} (max {} RPM)", addr, endpoint, rpm);

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|e| anyhow::anyhow!(e))?;

    println!("[monitor] Shutdown complete.");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to register SIGTERM");
        tokio::select! {
            _ = ctrl_c => { eprintln!("\n[monitor] Received Ctrl+C, shutting down..."); }
            _ = sigterm.recv() => { eprintln!("\n[monitor] Received SIGTERM, shutting down..."); }
        }
    }
    #[cfg(not(unix))]
    {
        ctrl_c.await.expect("failed to listen for ctrl+c");
        eprintln!("\n[monitor] Received Ctrl+C, shutting down...");
    }
}
