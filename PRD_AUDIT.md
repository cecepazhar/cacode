# CACode — Product Requirements Document & Audit

**Product:** CACode v0.3.0
**Author:** Cecep Saeful Azhar Hidayat, ST
**Last Updated:** 2026-10-25

## Functional Requirements

| ID | Requirement | Description | Status |
|----|-------------|-------------|--------|
| FR-01 | Chat Agent | Interactive chat session with ReAct agent loop for conversational coding assistance | Implemented |
| FR-02 | Tool Execution (Sandboxed) | Agent tools execute within a security sandbox — restricted paths, no network escalation | Implemented |
| FR-03 | Multi-Provider BYOK | Support OpenAI, Anthropic, Ollama, and 9Router with user-supplied API keys | Implemented |
| FR-04 | Token Monitor Proxy | Local HTTP proxy that intercepts API calls and logs token usage per request | Implemented |
| FR-05 | Usage Reports | Per-session and aggregate token/cost reports via `cacode report` | Implemented |
| FR-06 | TUI Interface | Rich terminal UI with input area, message history, and status bar | Implemented |
| FR-07 | Settings / Config | Configuration via `~/.cacode/config.toml` and `/settings` TUI command | Implemented |
| FR-08 | About | Display product name, version, author, license, and repository info | Implemented |
| FR-09 | Billing Info | Show current tier, pricing breakdown, and upgrade path | Implemented |
| FR-10 | File Operations (Sandboxed) | Read and write files within the allowed workspace directory only | Implemented |
| FR-11 | Bash Execution (Sandboxed + Confirmed) | Execute shell commands with user confirmation and sandbox restrictions | Implemented |
| FR-12 | Search Files | Regex and glob file search across the workspace | Implemented |
| FR-13 | Cost Estimation | Estimate cost before sending requests based on token count and provider pricing | Partial |
| FR-14 | Graceful Shutdown | Handle SIGINT/SIGTERM, flush pending writes, close DB connections cleanly | Implemented |
| FR-15 | Rate Limiting | Per-provider request rate limiting to avoid upstream throttling | Missing |

## Audit Summary

- **Implemented:** 13 / 15
- **Partial:** 1 (FR-13 — cost estimation pre-request hooks not yet wired)
- **Missing:** 1 (FR-15 — rate limiting not yet implemented)

## Next Steps

1. Wire pre-request cost estimation into the agent loop (FR-13)
2. Implement per-provider rate limiter middleware in the monitor proxy (FR-15)
3. Add integration tests for sandbox boundary enforcement
4. Publish v0.3.0 release artifacts
