use anyhow::Result;
use axum::Router;
use axum::extract::Extension;
use axum::http::{HeaderMap, Method, Uri, Response, StatusCode};
use axum::routing::any;
use hyper::body::Bytes;
use reqwest::Client;
use rusqlite::{params, Connection};
use serde_json::Value;
use std::net::SocketAddr;
use std::sync::Arc;
use chrono::Utc;
use std::collections::HashMap;
use dirs;
use axum::body::Body;

#[derive(Debug)]
struct Pricing {
    input_per_million: f64,
    output_per_million: f64,
}

fn default_pricing() -> HashMap<String, Pricing> {
    let mut map = HashMap::new();
    map.insert("gpt-4o".to_string(), Pricing { input_per_million: 5.0, output_per_million: 15.0 });
    map.insert("gpt-4o-mini".to_string(), Pricing { input_per_million: 0.15, output_per_million: 0.6 });
    map.insert("claude-3-sonnet-20240229".to_string(), Pricing { input_per_million: 3.0, output_per_million: 15.0 });
    map
}

#[derive(Clone)]
struct AppState {
    real_endpoint: String,
    client: Client,
    db_path: std::path::PathBuf,
    pricing: Arc<HashMap<String, Pricing>>,
}

async fn proxy_handler(
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    Extension(state): Extension<Arc<AppState>>,
    body: Bytes,
) -> Result<Response<axum::body::Body>, StatusCode> {
    // Build target URL
    let target_url = format!("{}{}", state.real_endpoint.trim_end_matches('/'), uri.path_and_query().map(|pq| pq.as_str()).unwrap_or(""));
    let client = &state.client;
    let mut req_builder = client.request(method.clone(), &target_url);
    // copy headers
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
    // Parse JSON body for usage
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
            let pricing = state.pricing.get(model).unwrap_or(&Pricing { input_per_million: 0.0, output_per_million: 0.0 });
            let est_cost = (prompt as f64 * pricing.input_per_million + completion as f64 * pricing.output_per_million) / 1_000_000.0;
            let ts = Utc::now().to_rfc3339();
            let db_path = state.db_path.clone();
            let model_str = model.to_string();
            let raw_str = String::from_utf8_lossy(&resp_bytes).to_string();
            tokio::task::spawn_blocking(move || {
                if let Ok(conn) = Connection::open(&db_path) {
                    let _ = conn.execute(
                        "INSERT INTO token_usage (timestamp, model, prompt_tokens, completion_tokens, total_tokens, est_cost, raw_response) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                        params![ts, model_str, prompt, completion, total, est_cost, raw_str],
                    );
                }
            });
        }
    }
    // Build response back to client
    let mut builder = Response::builder().status(status);
    for (name, value) in resp_headers.iter() {
        builder = builder.header(name, value);
    }
    let body = axum::body::Body::from(resp_bytes);
    let response = builder.body(body).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(response)
}

pub async fn run(endpoint: String, _costs: Option<String>) -> Result<()> {
    // Init DB path
    let db_path = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Home directory not found"))?
        .join(".cacode")
        .join("usage.db");
    std::fs::create_dir_all(db_path.parent().unwrap())?;
    // Ensure table exists
    let conn = Connection::open(&db_path)?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS token_usage (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp TEXT NOT NULL,
            model TEXT NOT NULL,
            prompt_tokens INTEGER NOT NULL,
            completion_tokens INTEGER NOT NULL,
            total_tokens INTEGER NOT NULL,
            est_cost REAL NOT NULL,
            raw_response TEXT
        )",
        [],
    )?;
    let state = Arc::new(AppState {
        real_endpoint: endpoint.clone(),
        client: Client::new(),
        db_path: db_path.clone(),
        pricing: Arc::new(default_pricing()),
    });
    let app = Router::new()
        .fallback(any(proxy_handler))
        .layer(Extension(state));
    let addr: SocketAddr = "127.0.0.1:8000".parse().unwrap();
    println!("[monitor] Starting proxy at {} forwarding to {}", addr, endpoint);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app)
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    Ok(())
}

