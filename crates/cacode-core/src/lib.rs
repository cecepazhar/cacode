//! CACode - Core library definitions.

pub mod commands;
pub mod config;
pub mod provider_registry;

pub use provider_registry::{
    auto_detect_best_free_provider, list_free_providers, probe_antigravity_zen, probe_ollama,
    select_best_ollama_model, AntigravityZenProbeResult, FreeProviderInfo, OllamaProbeResult,
    ANTIGRAVITY_ZEN_ENDPOINT, ANTIGRAVITY_ZEN_MODELS, DEFAULT_ANTIGRAVITY_ZEN_MODEL,
    DEFAULT_OLLAMA_URL,
};
