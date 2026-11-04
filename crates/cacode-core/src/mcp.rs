//! Minimal Model Context Protocol client (stdio transport, JSON-RPC 2.0).
//!
//! Config (Claude-compatible `mcpServers` shape), merged in this order:
//! `~/.cacode/mcp.json`, then — only in trusted workspaces — `<root>/.mcp.json`
//! and `<root>/.cacode/mcp.json`:
//!
//! ```json
//! { "mcpServers": { "fs": { "command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "."],
//!                            "env": {}, "trusted": false } } }
//! ```
//! Tools appear to the model as `mcp__<server>__<tool>`. Calls need approval
//! unless the tool declares `readOnlyHint` or the server is `"trusted": true`.
//! HTTP/SSE transports are not supported yet.

use crate::events::ApprovalRequest;
use crate::permission::ActionKind;
use crate::tools::{Approval, ToolContext, ToolSpec};
use anyhow::{anyhow, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::oneshot;

pub const PROTOCOL_VERSION: &str = "2024-11-05";
const START_TIMEOUT: Duration = Duration::from_secs(30);
const CALL_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ServerConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub trusted: bool,
    #[serde(default, rename = "type")]
    pub transport: Option<String>,
    #[serde(default)]
    pub disabled: bool,
}

#[derive(Debug, Default, Deserialize)]
struct ConfigFile {
    #[serde(default, rename = "mcpServers")]
    mcp_servers: BTreeMap<String, ServerConfig>,
}

pub fn load_configs(root: &Path, trusted: bool) -> BTreeMap<String, ServerConfig> {
    let mut files = Vec::new();
    if let Some(h) = dirs::home_dir() {
        files.push(h.join(".cacode").join("mcp.json"));
    }
    if trusted {
        files.push(root.join(".mcp.json"));
        files.push(root.join(".cacode").join("mcp.json"));
    }
    let mut out = BTreeMap::new();
    for f in files {
        if let Ok(text) = std::fs::read_to_string(&f) {
            match serde_json::from_str::<ConfigFile>(&text) {
                Ok(c) => out.extend(c.mcp_servers.into_iter().filter(|(_, s)| !s.disabled)),
                Err(e) => eprintln!("[cacode] ignoring invalid {}: {}", f.display(), e),
            }
        }
    }
    out
}

fn sanitize(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect()
}

pub fn tool_name(server: &str, tool: &str) -> String {
    let n = format!("mcp__{}__{}", sanitize(server), sanitize(tool));
    n.chars().take(64).collect()
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

pub struct McpClient {
    pub server: String,
    stdin: tokio::sync::Mutex<tokio::process::ChildStdin>,
    pending: Pending,
    next_id: AtomicU64,
    _child: tokio::sync::Mutex<tokio::process::Child>,
    alive: Arc<AtomicBool>,
}

impl McpClient {
    pub async fn spawn(server: &str, cfg: &ServerConfig, root: &Path) -> Result<Arc<Self>> {
        if let Some(t) = &cfg.transport {
            if t != "stdio" {
                return Err(anyhow!("transport '{}' not supported (stdio only)", t));
            }
        }
        let mut cmd = tokio::process::Command::new(&cfg.command);
        cmd.args(&cfg.args).envs(&cfg.env).current_dir(root).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null()).kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|e| anyhow!("cannot start MCP server '{}' ({}): {}", server, cfg.command, e))?;
        let stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
        let pending: Pending = Arc::default();
        let alive = Arc::new(AtomicBool::new(true));
        let client = Arc::new(Self { server: server.to_string(), stdin: tokio::sync::Mutex::new(stdin), pending: pending.clone(), next_id: AtomicU64::new(1), _child: tokio::sync::Mutex::new(child), alive: alive.clone() });

        // reader: responses → pending; server→client requests answered inline
        let weak = Arc::downgrade(&client);
        let root_uri = format!("file://{}", root.display());
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
                let id = msg.get("id").cloned();
                if msg.get("method").is_some() {
                    if let (Some(id), Some(c)) = (id, weak.upgrade()) {
                        let result = match msg["method"].as_str().unwrap_or("") {
                            "ping" => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
                            "roots/list" => json!({"jsonrpc": "2.0", "id": id, "result": {"roots": [{"uri": root_uri, "name": "workspace"}]}}),
                            _ => json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "method not supported by CACode"}}),
                        };
                        let _ = c.write(&result).await;
                    }
                    continue;
                }
                if let Some(n) = id.and_then(|i| i.as_u64()) {
                    if let Some(tx) = pending.lock().ok().and_then(|mut p| p.remove(&n)) {
                        let r = match msg.get("error") {
                            Some(e) => Err(e.get("message").and_then(|m| m.as_str()).unwrap_or("error").to_string()),
                            None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                        };
                        let _ = tx.send(r);
                    }
                }
            }
            alive.store(false, Ordering::SeqCst);
            if let Ok(mut p) = pending.lock() {
                for (_, tx) in p.drain() {
                    let _ = tx.send(Err("MCP server exited".into()));
                }
            }
        });
        Ok(client)
    }

    async fn write(&self, v: &Value) -> Result<()> {
        let mut s = self.stdin.lock().await;
        s.write_all(format!("{}\n", v).as_bytes()).await?;
        s.flush().await?;
        Ok(())
    }

    pub async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        if !self.alive.load(Ordering::SeqCst) {
            return Err(anyhow!("MCP server '{}' is not running", self.server));
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().map_err(|_| anyhow!("poisoned"))?.insert(id, tx);
        self.write(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})).await?;
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Ok(v))) => Ok(v),
            Ok(Ok(Err(e))) => Err(anyhow!("MCP {} error from '{}': {}", method, self.server, e)),
            Ok(Err(_)) => Err(anyhow!("MCP server '{}' dropped the request", self.server)),
            Err(_) => {
                if let Ok(mut p) = self.pending.lock() {
                    p.remove(&id);
                }
                Err(anyhow!("MCP {} timed out after {}s ('{}')", method, timeout.as_secs(), self.server))
            }
        }
    }

    pub async fn initialize(&self) -> Result<()> {
        self.request(
            "initialize",
            json!({"protocolVersion": PROTOCOL_VERSION, "capabilities": {"roots": {"listChanged": false}}, "clientInfo": {"name": "cacode", "version": env!("CARGO_PKG_VERSION")}}),
            START_TIMEOUT,
        )
        .await?;
        self.write(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).await
    }

    pub async fn list_tools(&self) -> Result<Vec<Value>> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..50 {
            let params = match &cursor { Some(c) => json!({"cursor": c}), None => json!({}) };
            let r = self.request("tools/list", params, START_TIMEOUT).await?;
            out.extend(r.get("tools").and_then(|t| t.as_array()).cloned().unwrap_or_default());
            cursor = r.get("nextCursor").and_then(|c| c.as_str()).map(String::from);
            if cursor.is_none() {
                break;
            }
        }
        Ok(out)
    }
}

/// Render MCP `content` blocks as text.
pub fn render_content(result: &Value) -> String {
    let mut parts = Vec::new();
    for b in result.get("content").and_then(|c| c.as_array()).cloned().unwrap_or_default() {
        match b.get("type").and_then(|t| t.as_str()) {
            Some("text") => parts.push(b.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string()),
            Some("image") => parts.push(format!("[image {} omitted]", b.get("mimeType").and_then(|m| m.as_str()).unwrap_or(""))),
            Some("resource") => parts.push(b.pointer("/resource/text").and_then(|t| t.as_str()).map(String::from).unwrap_or_else(|| "[binary resource omitted]".into())),
            _ => parts.push(b.to_string()),
        }
    }
    if parts.is_empty() {
        if let Some(sc) = result.get("structuredContent") {
            return sc.to_string();
        }
    }
    parts.join("\n")
}

struct Registered {
    server: String,
    tool: String,
    read_only: bool,
}

/// All MCP servers of a session; started lazily on first use.
pub struct McpManager {
    root: std::path::PathBuf,
    configs: BTreeMap<String, ServerConfig>,
    clients: tokio::sync::Mutex<HashMap<String, Arc<McpClient>>>,
    specs: Mutex<Vec<ToolSpec>>,
    index: Mutex<HashMap<String, Registered>>,
    started: tokio::sync::OnceCell<Vec<String>>,
}

impl std::fmt::Debug for McpManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "McpManager({:?})", self.configs.keys().collect::<Vec<_>>())
    }
}

impl McpManager {
    pub fn new(root: &Path, configs: BTreeMap<String, ServerConfig>) -> Option<Arc<Self>> {
        if configs.is_empty() {
            return None;
        }
        Some(Arc::new(Self {
            root: root.to_path_buf(),
            configs,
            clients: Default::default(),
            specs: Mutex::new(Vec::new()),
            index: Mutex::new(HashMap::new()),
            started: tokio::sync::OnceCell::new(),
        }))
    }

    /// Start every server once. Returns human-readable status lines.
    pub async fn ensure_started(&self) -> Vec<String> {
        self.started
            .get_or_init(|| async {
                let mut status = Vec::new();
                for (name, cfg) in &self.configs {
                    let r = async {
                        let c = McpClient::spawn(name, cfg, &self.root).await?;
                        c.initialize().await?;
                        let tools = c.list_tools().await?;
                        Ok::<_, anyhow::Error>((c, tools))
                    }
                    .await;
                    match r {
                        Ok((c, tools)) => {
                            let n = tools.len();
                            for t in tools {
                                let tname = t.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
                                if tname.is_empty() {
                                    continue;
                                }
                                let full = tool_name(name, &tname);
                                let ro = cfg.trusted || t.pointer("/annotations/readOnlyHint").and_then(|v| v.as_bool()).unwrap_or(false);
                                let schema = t.get("inputSchema").cloned().filter(|s| s.is_object()).unwrap_or_else(|| json!({"type": "object", "properties": {}}));
                                let desc = format!("[MCP {}] {}", name, t.get("description").and_then(|d| d.as_str()).unwrap_or(""));
                                if let Ok(mut s) = self.specs.lock() {
                                    s.push(ToolSpec { name: full.clone(), description: crate::text::truncate_chars(&desc, 1000), read_only: ro, schema });
                                }
                                if let Ok(mut ix) = self.index.lock() {
                                    ix.insert(full, Registered { server: name.clone(), tool: tname, read_only: ro });
                                }
                            }
                            self.clients.lock().await.insert(name.clone(), c);
                            status.push(format!("MCP '{}' connected ({} tools)", name, n));
                        }
                        Err(e) => status.push(format!("MCP '{}' failed: {}", name, e)),
                    }
                }
                status
            })
            .await
            .clone()
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.specs.lock().map(|s| s.clone()).unwrap_or_default()
    }

    pub async fn call(&self, ctx: &ToolContext, full_name: &str, args: &Value) -> Result<String> {
        self.ensure_started().await;
        let (server, tool, ro) = {
            let ix = self.index.lock().map_err(|_| anyhow!("poisoned"))?;
            let r = ix.get(full_name).ok_or_else(|| anyhow!("Unknown MCP tool {}", full_name))?;
            (r.server.clone(), r.tool.clone(), r.read_only)
        };
        let req = ApprovalRequest {
            id: 0,
            tool: full_name.into(),
            summary: format!("Run: {}", full_name),
            diff: Some(format!("MCP server '{}' → {}\narguments:\n{}", server, tool, serde_json::to_string_pretty(args).unwrap_or_default())),
        };
        match ctx.authorize(ActionKind::Exec, ro, req).await? {
            Approval::Granted => {}
            Approval::Denied(r) => return Err(anyhow!("Rejected MCP call {} — {}", full_name, r)),
        }
        let client = self.clients.lock().await.get(&server).cloned().ok_or_else(|| anyhow!("MCP server '{}' is not connected", server))?;
        let args = if args.is_object() { args.clone() } else { json!({}) };
        let fut = client.request("tools/call", json!({"name": tool, "arguments": args}), CALL_TIMEOUT);
        let result = tokio::select! {
            r = fut => r?,
            _ = ctx.cancel.cancelled() => return Err(anyhow!("Cancelled by user")),
        };
        let text = render_content(&result);
        if result.get("isError").and_then(|v| v.as_bool()).unwrap_or(false) {
            return Err(anyhow!("MCP tool {} reported an error: {}", full_name, text));
        }
        Ok(text)
    }
}
