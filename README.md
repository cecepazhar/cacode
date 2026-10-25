<p align="center">
  <strong>CACode — Sovereign Rust Development CLI & Chat Agent</strong>
</p>

<p align="center">
  <img alt="Version" src="https://img.shields.io/badge/version-0.3.0-blue?style=flat-square" />
  <img alt="License" src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-green?style=flat-square" />
  <img alt="Rust" src="https://img.shields.io/badge/Rust-2021-orange?style=flat-square&logo=rust" />
</p>

---

Full-lifecycle sovereign Rust development assistant with a **ReAct agent loop**, **token telemetry proxy**, **BYOK execution**, and **zero-telemetry** architecture. Your API keys stay on your machine — nothing phones home.

## Features

- **Interactive TUI Chat** — rich terminal interface for conversational coding assistance
- **ReAct Agent with 5 Tools** — autonomous reasoning loop: file read/write, bash execution, file search, cost estimation
- **Multi-Provider BYOK** — bring your own key for OpenAI, Anthropic, Ollama, or 9Router
- **Token Usage Monitor & Proxy** — local HTTP proxy that intercepts and logs every token consumed
- **Cost Estimation & Reports** — per-session and aggregate usage reports with cost breakdown
- **Sandboxed Tool Execution** — all file and shell operations run inside a security sandbox

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
cacode monitor      # Start token monitor proxy
cacode report       # Show usage reports
cacode about        # Product info and version
cacode billing      # Billing and tier details
```

## Configuration

CACode stores configuration at `~/.cacode/config.toml`. Set your BYOK provider keys there:

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

Or use `/settings` inside the TUI to configure interactively.

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
