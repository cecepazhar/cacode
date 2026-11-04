//! CACode - Core library definitions.

pub mod commands;
pub mod config;
pub mod events;
pub mod license_local;
pub mod hooks;
pub mod llm;
pub mod mcp;
pub mod permission;
pub mod pricing;
pub mod provider_registry;
pub mod repo_context;
pub mod search_replace;
pub mod sessions;
pub mod subagent;
pub mod symbols;
pub mod text;
pub mod tools;
pub mod trust;
pub mod types;
pub mod usage_db;
pub mod vault;
#[cfg(test)]
pub mod testutil;

pub use provider_registry::{
    auto_detect_best_free_provider, list_free_providers, probe_antigravity_zen, probe_ollama,
    select_best_ollama_model, AntigravityZenProbeResult, FreeProviderInfo, OllamaProbeResult,
    ANTIGRAVITY_ZEN_ENDPOINT, ANTIGRAVITY_ZEN_MODELS, DEFAULT_ANTIGRAVITY_ZEN_MODEL,
    DEFAULT_OLLAMA_URL,
};
pub use search_replace::*;
#[cfg(test)]
mod ext_tests;
