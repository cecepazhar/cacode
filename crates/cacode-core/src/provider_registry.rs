//! Free-Provider Registry and local daemon auto-detection.
//!
//! Provides probing, model discovery, and automatic provider selection for
//! zero-cost and local AI providers:
//! - Antigravity Zen (curated free models over OpenAI-compatible endpoint)
//! - Ollama (local daemon running on http://127.0.0.1:11434)

use std::env;
use std::time::Duration;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use crate::commands::chat::Provider;

pub const ANTIGRAVITY_ZEN_ENDPOINT: &str = "https://antigravity.ai/zen/v1/chat/completions";
pub const DEFAULT_ANTIGRAVITY_ZEN_MODEL: &str = "deepseek-v4-flash-free";
pub const ANTIGRAVITY_ZEN_MODELS: &[&str] = &[
    "deepseek-v4-flash-free",
    "mimo-v2.5-free",
    "big-pickle",
    "minimax-m2.5-free",
];

pub const DEFAULT_OLLAMA_URL: &str = "http://127.0.0.1:11434";
pub const DEFAULT_OLLAMA_PROBE_TIMEOUT_MS: u64 = 800;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OllamaTagModel {
    pub name: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OllamaTagsResponse {
    #[serde(default)]
    pub models: Vec<OllamaTagModel>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OllamaProbeResult {
    pub is_online: bool,
    pub url: String,
    pub models: Vec<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AntigravityZenProbeResult {
    pub is_available: bool,
    pub endpoint: String,
    pub models: Vec<String>,
    pub default_model: String,
    pub api_key_configured: bool,
    pub status_note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FreeProviderInfo {
    pub id: String,
    pub name: String,
    pub is_available: bool,
    pub endpoint_or_url: String,
    pub default_model: String,
    pub models: Vec<String>,
    pub note: String,
}

/// Probe local Ollama daemon tags endpoint with short timeout (default 800ms).
pub async fn probe_ollama(url: Option<&str>, timeout: Option<Duration>) -> OllamaProbeResult {
    let base_url = url
        .map(|s| s.to_string())
        .or_else(|| env::var("OLLAMA_URL").ok())
        .unwrap_or_else(|| DEFAULT_OLLAMA_URL.to_string());
    let base = base_url.trim_end_matches('/');
    let endpoint = format!("{}/api/tags", base);
    let probe_timeout = timeout.unwrap_or_else(|| Duration::from_millis(DEFAULT_OLLAMA_PROBE_TIMEOUT_MS));

    let client_res = Client::builder()
        .timeout(probe_timeout)
        .build();

    let client = match client_res {
        Ok(c) => c,
        Err(e) => {
            return OllamaProbeResult {
                is_online: false,
                url: base_url,
                models: Vec::new(),
                error: Some(format!("Failed to build HTTP client: {}", e)),
            };
        }
    };

    match client.get(&endpoint).send().await {
        Ok(resp) => {
            let status = resp.status();
            if status.is_success() {
                match resp.json::<OllamaTagsResponse>().await {
                    Ok(parsed) => {
                        let models: Vec<String> = parsed.models.into_iter().map(|m| m.name).collect();
                        OllamaProbeResult {
                            is_online: true,
                            url: base_url,
                            models,
                            error: None,
                        }
                    }
                    Err(e) => OllamaProbeResult {
                        is_online: true,
                        url: base_url,
                        models: Vec::new(),
                        error: Some(format!("Failed to parse /api/tags JSON: {}", e)),
                    },
                }
            } else {
                OllamaProbeResult {
                    is_online: false,
                    url: base_url,
                    models: Vec::new(),
                    error: Some(format!("Ollama daemon returned HTTP {}", status)),
                }
            }
        }
        Err(e) => OllamaProbeResult {
            is_online: false,
            url: base_url,
            models: Vec::new(),
            error: Some(e.to_string()),
        },
    }
}

/// Probe Antigravity Zen status and configuration.
pub fn probe_antigravity_zen(api_key: Option<&str>) -> AntigravityZenProbeResult {
    let key = api_key
        .map(|s| s.to_string())
        .or_else(|| env::var("ANTIGRAVITY_API_KEY").ok())
        .or_else(|| env::var("ANTIGRAVITY_ZEN_API_KEY").ok());

    let api_key_configured = key.as_ref().map(|k| !k.trim().is_empty()).unwrap_or(false);
    let status_note = if api_key_configured {
        "API key configured ✓ (Antigravity Zen curated tier)".to_string()
    } else {
        "Free tier available (no API key required or configure ANTIGRAVITY_API_KEY)".to_string()
    };

    AntigravityZenProbeResult {
        is_available: true,
        endpoint: ANTIGRAVITY_ZEN_ENDPOINT.to_string(),
        models: ANTIGRAVITY_ZEN_MODELS.iter().map(|s| s.to_string()).collect(),
        default_model: DEFAULT_ANTIGRAVITY_ZEN_MODEL.to_string(),
        api_key_configured,
        status_note,
    }
}

/// Select best Ollama model for coding tasks based on installed models list.
pub fn select_best_ollama_model(models: &[String]) -> String {
    if models.is_empty() {
        return "llama3".to_string();
    }

    // 1. Coding specialist models
    let coder_kw = [
        "qwen2.5-coder",
        "deepseek-coder",
        "codellama",
        "coder",
        "starcoder",
    ];
    for kw in &coder_kw {
        if let Some(m) = models.iter().find(|name| name.to_lowercase().contains(kw)) {
            return m.clone();
        }
    }

    // 2. High capability general models
    let general_kw = [
        "deepseek",
        "qwen",
        "llama3",
        "llama-3",
        "mistral",
        "gemma",
    ];
    for kw in &general_kw {
        if let Some(m) = models.iter().find(|name| name.to_lowercase().contains(kw)) {
            return m.clone();
        }
    }

    // 3. First available installed model
    models[0].clone()
}

/// List all available free and local providers with their probe status and models.
pub async fn list_free_providers(ollama_url: Option<&str>) -> Vec<FreeProviderInfo> {
    let mut list = Vec::new();

    // 1. Antigravity Zen
    let zen = probe_antigravity_zen(None);
    list.push(FreeProviderInfo {
        id: "antigravity_zen".into(),
        name: "Antigravity Zen".into(),
        is_available: true,
        endpoint_or_url: zen.endpoint,
        default_model: zen.default_model,
        models: zen.models,
        note: zen.status_note,
    });

    // 2. Ollama Local Daemon
    let ollama_res = probe_ollama(ollama_url, Some(Duration::from_millis(DEFAULT_OLLAMA_PROBE_TIMEOUT_MS))).await;
    let (note, default_model) = if ollama_res.is_online {
        let count = ollama_res.models.len();
        let best = select_best_ollama_model(&ollama_res.models);
        (format!("Daemon online ✓ ({} installed model{})", count, if count == 1 { "" } else { "s" }), best)
    } else {
        let err_desc = ollama_res.error.unwrap_or_else(|| "Connection refused / daemon not running".into());
        (format!("Daemon offline ({})", err_desc), "llama3".into())
    };

    list.push(FreeProviderInfo {
        id: "ollama".into(),
        name: "Ollama (Local)".into(),
        is_available: ollama_res.is_online,
        endpoint_or_url: ollama_res.url,
        default_model,
        models: ollama_res.models,
        note,
    });

    list
}

/// Auto-detect the best free provider when no paid keys (OpenAI, Anthropic, 9Router) exist.
///
/// Priority:
/// 1. Antigravity Zen if explicit key is provided.
/// 2. Local Ollama if daemon is online and has models installed.
/// 3. Antigravity Zen default free tier (deepseek-v4-flash-free).
pub async fn auto_detect_best_free_provider(ollama_url: Option<&str>) -> Provider {
    // 1. Check if Antigravity Zen key is explicitly set
    if env::var("ANTIGRAVITY_API_KEY").is_ok() || env::var("ANTIGRAVITY_ZEN_API_KEY").is_ok() {
        let model = env::var("ANTIGRAVITY_ZEN_MODEL")
            .or_else(|_| env::var("ANTIGRAVITY_MODEL"))
            .unwrap_or_else(|_| DEFAULT_ANTIGRAVITY_ZEN_MODEL.into());
        return Provider::AntigravityZen { model };
    }

    // 2. Check local Ollama daemon
    let ollama = probe_ollama(ollama_url, Some(Duration::from_millis(DEFAULT_OLLAMA_PROBE_TIMEOUT_MS))).await;
    if ollama.is_online && !ollama.models.is_empty() {
        let model = env::var("OLLAMA_MODEL").unwrap_or_else(|_| select_best_ollama_model(&ollama.models));
        return Provider::Ollama { model };
    }

    // 3. Fallback to Antigravity Zen free tier
    let model = env::var("ANTIGRAVITY_ZEN_MODEL")
        .or_else(|_| env::var("ANTIGRAVITY_MODEL"))
        .unwrap_or_else(|_| DEFAULT_ANTIGRAVITY_ZEN_MODEL.into());
    Provider::AntigravityZen { model }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_probe_antigravity_zen_defaults() {
        let res = probe_antigravity_zen(None);
        assert!(res.is_available);
        assert_eq!(res.endpoint, ANTIGRAVITY_ZEN_ENDPOINT);
        assert_eq!(res.default_model, "deepseek-v4-flash-free");
        assert_eq!(res.models.len(), 4);
        assert!(res.models.contains(&"deepseek-v4-flash-free".to_string()));
        assert!(res.models.contains(&"mimo-v2.5-free".to_string()));
        assert!(res.models.contains(&"big-pickle".to_string()));
        assert!(res.models.contains(&"minimax-m2.5-free".to_string()));
    }

    #[test]
    fn test_probe_antigravity_zen_with_key() {
        let res = probe_antigravity_zen(Some("zen-secret-key"));
        assert!(res.is_available);
        assert!(res.api_key_configured);
        assert!(res.status_note.contains("configured"));
    }

    #[test]
    fn test_select_best_ollama_model_coder_priority() {
        let models = vec![
            "llama3:latest".to_string(),
            "qwen2.5-coder:7b".to_string(),
            "mistral:latest".to_string(),
        ];
        let best = select_best_ollama_model(&models);
        assert_eq!(best, "qwen2.5-coder:7b");
    }

    #[test]
    fn test_select_best_ollama_model_general_priority() {
        let models = vec![
            "random-custom:v1".to_string(),
            "llama3:8b".to_string(),
        ];
        let best = select_best_ollama_model(&models);
        assert_eq!(best, "llama3:8b");
    }

    #[test]
    fn test_select_best_ollama_model_empty() {
        let models = vec![];
        let best = select_best_ollama_model(&models);
        assert_eq!(best, "llama3");
    }

    #[test]
    fn test_parse_ollama_tags_json() {
        let json = r#"{
            "models": [
                {
                    "name": "qwen2.5-coder:latest",
                    "model": "qwen2.5-coder:latest",
                    "size": 4661224676
                },
                {
                    "name": "llama3:latest",
                    "model": "llama3:latest",
                    "size": 4661224676
                }
            ]
        }"#;

        let parsed: OllamaTagsResponse = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.models.len(), 2);
        assert_eq!(parsed.models[0].name, "qwen2.5-coder:latest");
        assert_eq!(parsed.models[1].name, "llama3:latest");
    }

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[tokio::test]
    async fn test_probe_ollama_offline() {
        // Probe an unused port with 100ms timeout
        let res = probe_ollama(Some("http://127.0.0.1:59999"), Some(Duration::from_millis(100))).await;
        assert!(!res.is_online);
        assert!(res.models.is_empty());
        assert!(res.error.is_some());
    }

    #[tokio::test]
    async fn test_auto_detect_best_free_provider_fallback() {
        let _guard = ENV_LOCK.lock().unwrap();
        env::remove_var("ANTIGRAVITY_API_KEY");
        env::remove_var("ANTIGRAVITY_ZEN_API_KEY");
        env::remove_var("ANTIGRAVITY_ZEN_MODEL");
        env::remove_var("ANTIGRAVITY_MODEL");
        // Offline port for ollama
        let provider = auto_detect_best_free_provider(Some("http://127.0.0.1:59999")).await;
        match provider {
            Provider::AntigravityZen { model } => {
                assert_eq!(model, "deepseek-v4-flash-free");
            }
            _ => panic!("Expected Provider::AntigravityZen, got {:?}", provider),
        }
    }

    #[tokio::test]
    async fn test_auto_detect_best_free_provider_with_zen_key() {
        let _guard = ENV_LOCK.lock().unwrap();
        env::set_var("ANTIGRAVITY_API_KEY", "zen-key");
        env::set_var("ANTIGRAVITY_ZEN_MODEL", "minimax-m2.5-free");
        let provider = auto_detect_best_free_provider(None).await;
        match provider {
            Provider::AntigravityZen { model } => {
                assert_eq!(model, "minimax-m2.5-free");
            }
            _ => panic!("Expected Provider::AntigravityZen, got {:?}", provider),
        }
        env::remove_var("ANTIGRAVITY_API_KEY");
        env::remove_var("ANTIGRAVITY_ZEN_MODEL");
    }

    #[tokio::test]
    async fn test_list_free_providers() {
        let providers = list_free_providers(Some("http://127.0.0.1:59999")).await;
        assert_eq!(providers.len(), 2);
        assert_eq!(providers[0].id, "antigravity_zen");
        assert!(providers[0].is_available);
        assert_eq!(providers[1].id, "ollama");
        assert!(!providers[1].is_available);
    }
}
