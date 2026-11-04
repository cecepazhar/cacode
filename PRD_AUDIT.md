# CACode — Product Requirements Document & Audit

**Product:** CACode v1.2.0
**Author:** Cecep Saeful Azhar Hidayat, ST
**Last Updated:** 2026-11-03

Status is only "Implemented" when an automated test proves the behaviour (test name in the last column). Run `cargo test --workspace`.

## Functional Requirements

| ID | Requirement | Status | Evidence (test) |
|----|-------------|--------|-----------------|
| FR-01 | Chat agent — ReAct loop, native tool calling, observations persisted | Implemented | `chat::tests::react_loop_keeps_every_observation_native`, `audit_probes::probe_react_loop_message_sequence` |
| FR-02 | Tool sandbox — paths confined to the workspace, symlink escape blocked, no writes in `.git/` | Implemented | `tools::workspace::tests::*`, `tools::tests::read_is_numbered_paginated_and_sandboxed` |
| FR-03 | Multi-provider BYOK — OpenAI, Anthropic, Ollama, 9Router, Zen, custom (DeepSeek/OpenRouter/vLLM) | Implemented | `llm::tests::endpoint_resolution`, `llm::tests::anthropic_stream_tool_use` |
| FR-04 | Token monitor proxy — SSE passthrough, exact usage, fallback upstreams | Implemented | `monitor::tests::sse_is_streamed_through_and_usage_recorded`, `monitor::tests::failover_and_local_rate_limit` |
| FR-05 | Usage reports — model-aware cost, estimated/unknown-price flags | Implemented | `pricing::tests::real_model_ids_resolve` |
| FR-06 | TUI — independent tabs, streaming, status telemetry | Implemented | `runtime_tests::tab_switch_mid_turn_does_not_wedge_other_tabs` |
| FR-07 | Settings / config — secrets in encrypted vault, plaintext migration | Implemented | `config::tests::test_secrets_go_to_vault_not_toml`, `test_plaintext_config_is_migrated`, `vault::tests::*` |
| FR-08 | About | Implemented | — |
| FR-09 | Billing info (unshipped items marked "roadmap") | Implemented | — |
| FR-10 | File ops — numbered/paginated read, atomic write, must-read-first, stale detection, undo | Implemented | `tools::tests::edit_requires_read_and_detects_stale_file_then_undo` |
| FR-11 | Bash — permission gate (ask/auto-edit/yolo/read-only), timeout, process-group kill, background jobs, output truncation | Implemented | `tools::tests::ask_mode_without_frontend_denies_bash_but_allows_allowlisted`, `bash_exit_code_truncation_timeout_and_key_scrub`, `cancel_kills_whole_process_group`, `background_jobs` |
| FR-12 | Search — ignore-aware grep/glob, symbol lookup | Implemented | `tools::tests::grep_and_glob_respect_ignores`, `find_symbol_tool` |
| FR-13 | Cost estimation — pre-request token estimate drives auto-compaction | Implemented | `chat::tests::auto_compaction_keeps_request_under_window` |
| FR-14 | Graceful shutdown — panic hook restores terminal, cancel keeps history | Implemented | `runtime_tests::ctrl_c_keeps_history_of_cancelled_turn` |
| FR-15 | Rate limiting / resilience — backoff, Retry-After, breaker, timeouts | Implemented | `chat::tests::rate_limit_honours_retry_after`, `breaker_skips_failed_primary_then_recovers_after_cooldown`, `hung_provider_times_out_and_fails_over` |
| FR-16 | Diff approval before applying edits | Implemented | `tools::tests::approval_flow_reject_leaves_file_untouched`, `runtime_tests::approval_dialog_applies_edit_only_after_yes`, `ui_renders_approval_dialog_with_diff` |
| FR-17 | Parallel subagents with evidence-based reports | Implemented | `chat::tests::parallel_subagents_respect_semaphore_and_report_evidence`, `subagent_claim_without_evidence_is_flagged` |
| FR-18 | Headless mode (`-p`, json / stream-json, exit codes) | Implemented | `tests/headless.rs::headless_json_edit_then_continue_session`, `headless_stream_json_and_exit_codes` |
| FR-19 | Session persistence & resume (`--continue`, `--resume`, `/resume`) | Implemented | `sessions::tests::save_list_load_latest`, `headless_json_edit_then_continue_session` |
| FR-20 | MCP client (stdio, pagination, permissions) | Implemented | `ext_tests::mcp_tools_list_paginate_call_and_permissions`, `agent_calls_mcp_tool_end_to_end` |
| FR-21 | Hooks (pre/post tool) + workspace trust | Implemented | `ext_tests::hooks_block_annotate_and_fail`, `untrusted_workspace_ignores_project_hooks_and_mcp` |
| FR-22 | OS sandbox for bash (bubblewrap) | Implemented (Linux) | `tools::tests::bwrap_sandbox_confines_writes_to_workspace` |
| FR-23 | Plan mode + agent todo list | Implemented | `tools::tests::plan_mode_blocks_edits_until_plan_approved`, `todo_write_validates_and_emits` |
| FR-24 | web_fetch with domain allowlist | Implemented | `tools::tests::web_fetch_converts_html_and_gates_domains` |
| FR-25 | Anthropic prompt caching + cache-aware cost | Implemented | `llm::tests::anthropic_prompt_caching_breakpoints`, `cache_usage_parsed_and_priced` |
| FR-26 | tree-sitter symbol index | Implemented | `symbols::tests::tree_sitter_handles_what_regex_misses` |
| FR-27 | Real-provider validation | **Pending** | run `scripts/e2e_real.sh` and attach `report.md` |

## Known limitations (honest list)

- The bash OS sandbox needs Linux + bubblewrap; on macOS/Windows (or without bwrap, `sandbox = "auto"`) commands are only permission-gated.
- MCP: stdio transport only (no HTTP/SSE servers yet).
- Everything above is verified against mock providers; real-provider results come from `scripts/e2e_real.sh`.
- The code highlighter in the TUI is keyword-based, not a full grammar.
- Built-in model prices must be verified against provider pages; override in `~/.cacode/pricing.toml`.
- Ollama's OpenAI endpoint uses the server's default `num_ctx`; set a larger context in the Ollama Modelfile for long sessions.
- License keys are validated offline by checksum only (not cryptographically signed).
- Roadmap (not shipped): cross-session memory sync, dual-agent debate loops.
