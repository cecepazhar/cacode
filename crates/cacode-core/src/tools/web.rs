//! `web_fetch`: read a documentation page / API response as text.
//! Domains outside the allowlist need approval (ApproveAlways = whole domain
//! for the session). Only http/https, ≤ 5 MB, 20 s timeout, ≤ 5 redirects.

use super::{Approval, ToolContext};
use crate::events::ApprovalRequest;
use crate::permission::ActionKind;
use anyhow::{anyhow, Result};
use std::time::Duration;

pub const DEFAULT_WEB_ALLOWLIST: &[&str] = &[
    "docs.rs", "crates.io", "doc.rust-lang.org", "github.com", "raw.githubusercontent.com", "gist.githubusercontent.com",
    "pkg.go.dev", "go.dev", "developer.mozilla.org", "docs.python.org", "pypi.org", "www.npmjs.com", "learn.microsoft.com",
    "docs.anthropic.com", "docs.claude.com", "platform.openai.com", "htmx.org", "tailwindcss.com", "echo.labstack.com", "templ.guide",
];
const MAX_BYTES: usize = 5 * 1024 * 1024;

pub fn domain_allowed(host: &str, allow: &[String]) -> bool {
    let h = host.trim_start_matches("www.").to_lowercase();
    allow.iter().any(|d| {
        let d = d.trim_start_matches("www.").to_lowercase();
        h == d || h.ends_with(&format!(".{}", d))
    })
}

pub fn html_to_text(html: &[u8]) -> String {
    html2text::from_read(html, 100).unwrap_or_else(|_| String::from_utf8_lossy(html).to_string())
}

pub async fn web_fetch(ctx: &ToolContext, args: &serde_json::Value) -> Result<String> {
    let raw = args.get("url").and_then(|v| v.as_str()).ok_or_else(|| anyhow!("Missing url"))?;
    let url = url::Url::parse(raw).map_err(|e| anyhow!("invalid url '{}': {}", raw, e))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(anyhow!("only http/https URLs are allowed"));
    }
    let host = url.host_str().ok_or_else(|| anyhow!("url has no host"))?.to_string();
    let allowed = domain_allowed(&host, &ctx.web_allowlist);
    let req = ApprovalRequest { id: 0, tool: "web_fetch".into(), summary: format!("Run: web_fetch {}", host), diff: Some(format!("GET {}", url)) };
    match ctx.authorize(ActionKind::Exec, allowed, req).await? {
        Approval::Granted => {}
        Approval::Denied(r) => return Err(anyhow!("Rejected fetch of {} — {}", host, r)),
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::limited(5))
        .user_agent(concat!("CACode/", env!("CARGO_PKG_VERSION"), " (+https://github.com/cecepazhar/cacode)"))
        .build()?;
    let resp = tokio::select! {
        r = client.get(url.clone()).send() => r.map_err(|e| anyhow!("fetch failed: {}", e))?,
        _ = ctx.cancel.cancelled() => return Err(anyhow!("Cancelled by user")),
    };
    let status = resp.status();
    let final_url = resp.url().to_string();
    let ctype = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_lowercase();
    let mut body = Vec::new();
    let mut stream = resp.bytes_stream();
    use futures_util::StreamExt;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| anyhow!("read failed: {}", e))?;
        body.extend_from_slice(&chunk);
        if body.len() > MAX_BYTES {
            body.truncate(MAX_BYTES);
            break;
        }
    }
    let text = if ctype.contains("html") || (ctype.is_empty() && body.starts_with(b"<")) {
        html_to_text(&body)
    } else if ctype.starts_with("text/") || ctype.contains("json") || ctype.contains("xml") || ctype.contains("javascript") || ctype.is_empty() {
        if crate::text::looks_binary(&body) {
            return Ok(format!("{} {} — binary content ({} bytes, {}) not shown", status.as_u16(), final_url, body.len(), ctype));
        }
        String::from_utf8_lossy(&body).to_string()
    } else {
        return Ok(format!("{} {} — non-text content ({} bytes, {}) not shown", status.as_u16(), final_url, body.len(), ctype));
    };
    Ok(format!("[{} {} · {}]\n{}", status.as_u16(), final_url, if ctype.is_empty() { "unknown type" } else { &ctype }, crate::text::clamp_output(&text, 24_000, 600)))
}
