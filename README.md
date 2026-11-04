<p align="center">
  <strong>CACode — Sovereign Rust Development CLI & Chat Agent</strong>
</p>

<p align="center">
  <img alt="Version" src="https://img.shields.io/badge/version-1.2.0-blue?style=flat-square" />
  <img alt="License" src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-green?style=flat-square" />
  <img alt="Rust" src="https://img.shields.io/badge/Rust-2021-orange?style=flat-square&logo=rust" />
</p>

---

Full-lifecycle sovereign Rust development assistant with a **ReAct agent loop**, **token telemetry proxy**, **BYOK execution**, and **zero-telemetry** architecture. Your API keys stay on your machine — nothing phones home.

## Features

- **ReAct agent with native tool calling** — OpenAI-compatible (OpenAI, Ollama `/v1`, 9Router, Antigravity Zen, DeepSeek, OpenRouter, vLLM, any custom endpoint) and Anthropic Messages API, streamed (SSE). Weak models without tool support are downgraded automatically to a JSON-fence text mode.
- **Resilient routing** — HTTP timeouts (first byte / idle), typed errors, exponential backoff with jitter, `Retry-After` honoured, 3-tier fallback chain and a per-provider circuit breaker (the primary is retried after a cooldown, never replaced permanently).
- **Sandboxed tools** — `read_file` (numbered, paginated), `write_file` / `edit_file` (atomic write, must-read-first, stale-file detection, unique-match patching, `/undo`), `bash` (timeout, exit code, background jobs, process-group kill, API keys scrubbed from the env), `grep` / `glob` / `list_dir` (respect `.gitignore`, `.ignore`, `.cacodeignore`), `find_symbol`, `task` (subagents).
- **Permission modes** — `ask` (diff approval dialog), `auto-edit`, `yolo`, `read-only` (`/permissions`, `CACODE_PERMISSION_MODE`).
- **Parallel subagents** — `task` tool, fresh context, read-only by default, 5 concurrent (Free) / 49 (Pro), step / token / time budgets; reports carry evidence from commands that actually ran and are flagged `UNVERIFIED` otherwise.
- **Long conversations** — auto-compaction at 80 % of the model window, provider "context too long" errors trigger compaction + retry, `/compress` runs in the background.
- **Repo context** — cwd, git branch/status/log, ignore-aware tree, `CACODE.md` / `AGENTS.md` project instructions and a symbol map injected into the system prompt.
- **TUI** — independent tabs, streaming output, approval/diff dialog, `@file` mentions, Ctrl+R history search, persistent history, bracketed paste folding, code highlighting, live status (elapsed / active tool / tok/s).
- **Token Usage Monitor & Proxy** — `cacode monitor` streams SSE through without buffering, records exact usage, `--fallback` upstreams, local RPM limit with `Retry-After`.
- **Headless & sessions** — `cacode -p "task" --output-format json|stream-json` for CI and other agents (exit codes 0/1/2/130); every conversation is saved, `cacode --continue` / `--resume <id>` / `/resume`.
- **MCP client** — stdio MCP servers from `~/.cacode/mcp.json` or a trusted project's `.mcp.json` (Claude-compatible `mcpServers` format); tools appear as `mcp__<server>__<tool>`.
- **Hooks** — `pre_tool` / `post_tool` shell hooks (`~/.cacode/hooks.toml`, project `.cacode/hooks.toml` when trusted); exit 2 blocks.
- **Plan mode** — `/plan` or `--permission-mode plan`: read-only investigation, then the agent asks you to approve a plan.
- **OS sandbox** — on Linux with bubblewrap, `bash` runs with the filesystem read-only except the workspace, a private /tmp and package caches (`sandbox = "auto" | "required" | "off"`).
- **todo_write, web_fetch, prompt caching** — agent-managed task list in the TUI, docs fetching with a domain allowlist, Anthropic prompt caching with cache-aware cost.
- **tree-sitter symbol index** — Rust, Go, TS/TSX/JS, Python, C# (regex fallback for files with syntax errors).
- **Model-aware cost reports** — one pricing table (`~/.cacode/pricing.toml` overrides); estimated counts and unknown prices are flagged instead of silently reported.

## Tiers

| Tier | Price | Includes |
|------|-------|----------|
| **Free Core** | $0 | BYOK chat agent, CLI tools, local monitor |
| **Pro Sovereign** | $39 | Priority models, advanced reports, workspace sync |
| **Ultra Pro** | $89 | Full autonomy suite, team dashboards, SLA support |

## Installation

```bash
cargo install --path crates/cacode-app
```

## Usage

```bash
cacode              # Launch TUI interface
cacode chat         # Start chat session
cacode monitor --endpoint https://api.openai.com --fallback http://localhost:20128   # token proxy on :8000
cacode report       # Show usage reports
cacode about        # Product info and version
cacode billing      # Billing and tier details
```

## Configuration

CACode stores configuration at `~/.cacode/config.toml`. API keys are moved automatically into an encrypted vault (`~/.cacode/secrets.enc`, ChaCha20-Poly1305, key in `~/.cacode/vault.key`, mode 0600) — plaintext keys found in an old config are migrated on first load. The vault keeps keys out of dotfile repos, backups and screen shares; it does not protect against malware running as your user.

Keys can still be provided via environment variables or the TOML below (they will be migrated):

```toml
[provider]
default = "openai"

[provider.openai]
api_key = "sk-..."
model = "gpt-4o"

[provider.anthropic]
api_key = "sk-ant-..."
model = "claude-sonnet-4-20250514"

[provider.ollama]
base_url = "http://localhost:11434"
model = "llama3"
```

Or use `/settings` inside the TUI to configure interactively. Other useful keys: `permission_mode = "ask"`, `max_steps = 25`, `context_window = 0` (0 = auto from the pricing table).

Per-project instructions: put a `CACODE.md` in the repo root. Exclude paths from the agent with `.cacodeignore` (gitignore syntax).

## Headless / CI

```bash
cacode -p "fix the failing test" --permission-mode auto-edit --output-format json
cat task.md | cacode -p - --output-format stream-json
cacode --continue -p "now add a test for it"
cacode sessions            # list saved sessions here (--all for every directory)
```

## Real-provider end-to-end test

```bash
ANTHROPIC_API_KEY=... DEEPSEEK_API_KEY=... scripts/e2e_real.sh
scripts/e2e_real.sh ollama          # only one provider
```
Runs 7 tasks (question, bug fix, feature + test, multi-file rename, subagents, resume, web_fetch) per provider in throw-away fixtures with an isolated HOME and writes `e2e-results/<timestamp>/report.md`.

## Development

```bash
cargo test --workspace      # 132 tests incl. mock-provider end-to-end agent tests
cargo clippy --workspace --all-targets
```

## Author

**Cecep Saeful Azhar Hidayat, ST**

- Web: [cecepazhar.com](https://cecepazhar.com)
- Email: [hi@cecepazhar.com](mailto:hi@cecepazhar.com)
- GitHub: [@cecepazhar](https://github.com/cecepazhar)

## License

Licensed under either of:

- **MIT License** — see [LICENSE-MIT](LICENSE-MIT)
- **Apache License 2.0** — see [LICENSE-APACHE](LICENSE-APACHE)

at your option.
