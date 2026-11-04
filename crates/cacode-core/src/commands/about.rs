use anyhow::Result;

pub fn run() -> Result<()> {
    println!("=== CACode: Full‑Lifecycle Sovereign Rust CLI & Chat‑Agent ===");
    println!();
    println!("Author:  Cecep Saeful Azhar Hidayat, ST (cecepazhar)");
    println!("Version: {}", env!("CARGO_PKG_VERSION"));
    println!("License: MIT OR Apache-2.0");
    println!("Repo:    https://github.com/cecepazhar/cacode");
    println!();
    println!("Positioning & Vision:");
    println!("  CACode merges a full-lifecycle software development assistant with");
    println!("  a chat REPL interface, native token telemetry proxy, and sovereign");
    println!("  BYOK execution with strict zero-telemetry.");
    println!();
    println!("Key Features:");
    println!("  • ReAct agent with native tool calling (OpenAI-compatible + Anthropic), streaming,");
    println!("    retry/backoff, fallback routing and a per-provider circuit breaker.");
    println!("  • Sandboxed tools: read/write/edit (stale-file check, diff approval, /undo),");
    println!("    bash (timeouts, background jobs, process-group kill), grep/glob/find_symbol.");
    println!("  • Permission modes: ask · auto-edit · yolo · read-only.");
    println!("  • Auto-compaction of long conversations; repo context + symbol map.");
    println!("  • Local token usage monitor/proxy (:8000) & model-aware cost reports.");
    println!("  • Multi-Tab sessions running independently (Pro).");
    println!("  • Parallel subagents via the `task` tool: 5 concurrent (Free), 49 (Pro).");
    println!("  • Thinking modes Fast / Analytical / Deep (prompt-level guidance).");
    println!();
    Ok(())
}
