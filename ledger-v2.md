# CACode — Development Ledger v2

Catatan perubahan pengembangan produk CACode.

| No | Tanggal | Versi | Perubahan | Kategori |
|----|---------|-------|-----------|----------|
| 1 | 2026-10-20 | v0.1.0 | Initial release — CLI chat agent, perintah about dan billing | Release |
| 2 | 2026-10-22 | v0.2.0 | Ditambahkan monitor proxy, perintah report, dukungan multi-provider (OpenAI, Anthropic, Ollama, 9Router) | Feature |
| 3 | 2026-10-25 | v0.3.0 | Restrukturisasi workspace, antarmuka TUI, pengaturan BYOK, sandbox keamanan, penguatan monitor | Feature |
| 4 | 2026-11-03 | v1.1.0 | Audit fixes: native tool calling + streaming, history/observation fix, typed provider errors + backoff + circuit breaker + timeouts, auto-compaction, workspace sandbox + permission modes + diff approval + /undo, safe patch engine, bash v2 (timeout/background/process-group kill), ignore-aware grep/glob, symbol index + repo context, parallel subagents with evidence reports, streaming proxy with fallback, model-aware pricing, encrypted key vault, per-tab turn state, TUI history/paste/@file/highlighting; 117 tests | Feature/Fix |
| 5 | 2026-11-03 | v1.2.0 | Headless mode (-p, json/stream-json) + session store (--continue/--resume//resume), MCP stdio client, pre/post tool hooks + workspace trust, bubblewrap bash sandbox, plan mode, todo_write, web_fetch, Anthropic prompt caching + cache-aware cost, tree-sitter symbols, scripts/e2e_real.sh; fixes: REPL hang after first turn, read-state lost between TUI turns; 132 tests | Feature/Fix |
