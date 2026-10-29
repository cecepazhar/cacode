use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CustomProviderConfig {
    pub id: String,
    pub name: String,
    pub base_url: String,
    pub api_key: String,
    pub model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaCodeConfig {
    #[serde(default)]
    pub openai_api_key: Option<String>,
    #[serde(default)]
    pub openai_base_url: Option<String>,
    #[serde(default)]
    pub anthropic_api_key: Option<String>,
    #[serde(default)]
    pub ollama_url: Option<String>,
    #[serde(default)]
    pub ollama_model: Option<String>,
    #[serde(default)]
    pub router9_url: Option<String>,
    #[serde(default)]
    pub router9_api_key: Option<String>,
    #[serde(default)]
    pub router9_model: Option<String>,
    #[serde(default)]
    pub antigravity_zen_api_key: Option<String>,
    #[serde(default = "default_antigravity_zen_model")]
    pub antigravity_zen_model: Option<String>,
    #[serde(default = "default_user_name")]
    pub user_name: String,
    #[serde(default = "default_persona")]
    pub system_persona: String,
    #[serde(default)]
    pub thinking_mode: String,
    #[serde(default = "default_theme")]
    pub theme: String,
    #[serde(default = "default_app_name")]
    pub app_name: String,
    #[serde(default)]
    pub pro_license_key: Option<String>,
    #[serde(default)]
    pub custom_providers: Vec<CustomProviderConfig>,
    #[serde(default)]
    pub fallback_1: Option<String>,
    #[serde(default)]
    pub fallback_2: Option<String>,
    #[serde(default)]
    pub fallback_3: Option<String>,
    #[serde(default = "default_max_retries")]
    pub max_retries: usize,
}

fn default_max_retries() -> usize { 3 }
fn default_theme() -> String { "teal".into() }
fn default_app_name() -> String { "CACode — Sovereign Dev Agent".into() }
fn default_user_name() -> String { "Prof. Cecep".into() }
fn default_antigravity_zen_model() -> Option<String> { Some("deepseek-v4-flash-free".into()) }
fn default_persona() -> String {
    "You are CACode, a sovereign Rust-focused development agent. The user is Prof. Cecep (Cecep Saeful Azhar Hidayat, ST). Always greet and address him respectfully as 'Prof. Cecep' (e.g. 'Hi Prof. Cecep', 'Siap, Prof. Cecep.'). Be direct, technically precise, and concise. Provide working code and verified architecture solutions.".into()
}

impl Default for CaCodeConfig {
    fn default() -> Self {
        Self {
            openai_api_key: None,
            openai_base_url: None,
            anthropic_api_key: None,
            ollama_url: None,
            ollama_model: None,
            router9_url: None,
            router9_api_key: None,
            router9_model: Some("ZA126_PRO".into()),
            antigravity_zen_api_key: None,
            antigravity_zen_model: default_antigravity_zen_model(),
            user_name: default_user_name(),
            system_persona: default_persona(),
            thinking_mode: "Fast".into(),
            theme: default_theme(),
            app_name: default_app_name(),
            pro_license_key: None,
            custom_providers: Vec::new(),
            fallback_1: None,
            fallback_2: None,
            fallback_3: None,
            max_retries: default_max_retries(),
        }
    }
}

impl CaCodeConfig {
    pub fn is_pro(&self) -> bool {
        match self.pro_license_key.as_ref() {
            Some(k) if !k.trim().is_empty() => parse_ca_license_key(k).is_some(),
            _ => false,
        }
    }

    /// Validate a CA license key in the new format: CA[PRODUCT][TIER]-[RAND6]-[CK]
    pub fn validate_license_key(key: &str) -> bool {
        parse_ca_license_key(key).is_some()
    }
}

/// Parse and validate a CA license key.
/// Returns `Some((product, tier))` on success, `None` on failure.
fn parse_ca_license_key(key: &str) -> Option<(String, String)> {
    let upper = key.trim().to_uppercase();
    let parts: Vec<&str> = upper.split('-').collect();
    if parts.len() != 3 { return None; }
    let prefix_tier = parts[0];
    let rand_part = parts[1];
    let ck_str = parts[2];
    if prefix_tier.len() != 6 { return None; }
    if rand_part.len() != 6 { return None; }
    if ck_str.len() != 1 { return None; }
    if !prefix_tier.starts_with("CA") { return None; }
    let product = &prefix_tier[2..4];
    let tier = &prefix_tier[4..6];
    let allowed = ["TM","MK","CD","SH","TB","PS","UI","AA","CF","CB"];
    if !allowed.contains(&product) { return None; }
    if tier.parse::<u8>().is_err() { return None; }
    if !rand_part.chars().all(|c| c.is_ascii_alphanumeric()) { return None; }
    // Compute checksum (mod‑10)
    let mut sum: u32 = 0;
    for ch in upper[..upper.len()-2].chars() {
        let val = if ch.is_ascii_digit() {
            (ch as u8 - b'0') as u32
        } else if ch.is_ascii_uppercase() {
            (ch as u8 - b'A' + 10) as u32
        } else {
            0
        };
        sum += val;
    }
    let expected_ck = ((10 - (sum % 10)) % 10) as u8;
    let provided_ck = ck_str.parse::<u8>().ok()?;
    if expected_ck != provided_ck { return None; }
    Some((product.to_string(), tier.to_string()))
}

    pub fn config_path() -> Result<PathBuf> {
        let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Home directory not found"))?;
        Ok(home.join(".cacode").join("config.toml"))
    }

    pub fn load() -> Result<Self> {
        let path = Self::config_path()?;
        if path.exists() {
            let contents = fs::read_to_string(&path)?;
            let cfg: CaCodeConfig = toml::from_str(&contents)?;
            Ok(cfg)
        } else {
            Ok(Self::default())
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::config_path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let contents = toml::to_string_pretty(self)?;
        fs::write(&path, contents)?;
        Ok(())
    }

    pub fn apply_to_env(&self) {
        let mappings: &[(&str, &Option<String>)] = &[
            ("OPENAI_API_KEY", &self.openai_api_key),
            ("OPENAI_BASE_URL", &self.openai_base_url),
            ("ANTHROPIC_API_KEY", &self.anthropic_api_key),
            ("OLLAMA_URL", &self.ollama_url),
            ("OLLAMA_MODEL", &self.ollama_model),
            ("ROUTER9_URL", &self.router9_url),
            ("ROUTER9_API_KEY", &self.router9_api_key),
            ("ROUTER9_MODEL", &self.router9_model),
            ("ANTIGRAVITY_API_KEY", &self.antigravity_zen_api_key),
            ("ANTIGRAVITY_ZEN_API_KEY", &self.antigravity_zen_api_key),
            ("ANTIGRAVITY_ZEN_MODEL", &self.antigravity_zen_model),
        ];
        for (var, val) in mappings {
            if let Some(v) = val {
                if !v.is_empty() {
                    env::set_var(var, v);
                }
            }
        }
    }

    pub fn effective(&self, env_name: &str, config_val: &Option<String>) -> String {
        config_val.clone().unwrap_or_else(|| env::var(env_name).unwrap_or_default())
    }

    pub fn effective_antigravity_api_key(&self) -> String {
        self.antigravity_zen_api_key
            .clone()
            .or_else(|| env::var("ANTIGRAVITY_API_KEY").ok())
            .or_else(|| env::var("ANTIGRAVITY_ZEN_API_KEY").ok())
            .unwrap_or_default()
    }

    pub fn effective_antigravity_model(&self) -> String {
        self.antigravity_zen_model
            .clone()
            .or_else(|| env::var("ANTIGRAVITY_ZEN_MODEL").ok())
            .or_else(|| env::var("ANTIGRAVITY_MODEL").ok())
            .unwrap_or_else(|| "deepseek-v4-flash-free".into())
    }

    pub fn display_fields(&self) -> Vec<(&'static str, &'static str, &'static str, String)> {
        vec![
            ("App Name", "CACODE_APP_NAME", "app_name", self.app_name.clone()),
            ("Theme Style", "CACODE_THEME", "theme", self.theme.clone()),
            ("Thinking Mode", "CACODE_THINKING_MODE", "thinking_mode", self.thinking_mode.clone()),
            ("User Name (Greeting)", "CACODE_USER_NAME", "user_name", self.user_name.clone()),
            ("Pro License Key", "CACODE_PRO_LICENSE", "pro_license_key", Self::mask(&self.pro_license_key.clone().unwrap_or_default())),
            ("Fallback Tier 1", "CACODE_FALLBACK_1", "fallback_1", self.fallback_1.clone().unwrap_or_else(|| "(none)".into())),
            ("Fallback Tier 2", "CACODE_FALLBACK_2", "fallback_2", self.fallback_2.clone().unwrap_or_else(|| "(none)".into())),
            ("Fallback Tier 3", "CACODE_FALLBACK_3", "fallback_3", self.fallback_3.clone().unwrap_or_else(|| "(none)".into())),
            ("Max Retries", "CACODE_MAX_RETRIES", "max_retries", self.max_retries.to_string()),
            ("Custom Providers", "CACODE_CUSTOM_PROVIDERS", "custom_providers", format!("{} configured", self.custom_providers.len())),
            ("OpenAI API Key", "OPENAI_API_KEY", "openai_api_key", Self::mask(&self.effective("OPENAI_API_KEY", &self.openai_api_key))),
            ("OpenAI Base URL", "OPENAI_BASE_URL", "openai_base_url", self.effective("OPENAI_BASE_URL", &self.openai_base_url)),
            ("Anthropic API Key", "ANTHROPIC_API_KEY", "anthropic_api_key", Self::mask(&self.effective("ANTHROPIC_API_KEY", &self.anthropic_api_key))),
            ("Ollama URL", "OLLAMA_URL", "ollama_url", self.effective("OLLAMA_URL", &self.ollama_url)),
            ("Ollama Model", "OLLAMA_MODEL", "ollama_model", self.effective("OLLAMA_MODEL", &self.ollama_model)),
            ("9Router URL", "ROUTER9_URL", "router9_url", self.effective("ROUTER9_URL", &self.router9_url)),
            ("9Router API Key", "ROUTER9_API_KEY", "router9_api_key", Self::mask(&self.effective("ROUTER9_API_KEY", &self.router9_api_key))),
            ("9Router Model", "ROUTER9_MODEL", "router9_model", self.effective("ROUTER9_MODEL", &self.router9_model)),
            ("Antigravity Zen Key", "ANTIGRAVITY_API_KEY", "antigravity_zen_api_key", Self::mask(&self.effective_antigravity_api_key())),
            ("Antigravity Zen Model", "ANTIGRAVITY_ZEN_MODEL", "antigravity_zen_model", self.effective_antigravity_model()),
        ]
    }

    pub fn mask(val: &str) -> String {
        if val.is_empty() {
            "(not set)".to_string()
        } else if val.len() <= 4 {
            "****".to_string()
        } else {
            format!("{}…{}", "*".repeat(val.len().min(12) - 4), &val[val.len() - 4..])
        }
    }

    pub fn set_field(&mut self, key: &str, value: String) {
        let val = if value.is_empty() { None } else { Some(value) };
        match key {
            "app_name" => { if let Some(v) = val { self.app_name = v; } }
            "theme" => { if let Some(v) = val { self.theme = v; } }
            "thinking_mode" => { if let Some(v) = val { self.thinking_mode = v; } }
            "user_name" => { if let Some(v) = val { self.user_name = v; } }
            "system_persona" => { if let Some(v) = val { self.system_persona = v; } }
            "pro_license_key" => self.pro_license_key = val,
            "fallback_1" => self.fallback_1 = val,
            "fallback_2" => self.fallback_2 = val,
            "fallback_3" => self.fallback_3 = val,
            "max_retries" => {
                if let Some(v) = val {
                    if let Ok(n) = v.parse::<usize>() {
                        self.max_retries = n;
                    }
                }
            }
            "openai_api_key" => self.openai_api_key = val,
            "openai_base_url" => self.openai_base_url = val,
            "anthropic_api_key" => self.anthropic_api_key = val,
            "ollama_url" => self.ollama_url = val,
            "ollama_model" => self.ollama_model = val,
            "router9_url" => self.router9_url = val,
            "router9_api_key" => self.router9_api_key = val,
            "router9_model" => self.router9_model = val,
            "antigravity_zen_api_key" => self.antigravity_zen_api_key = val,
            "antigravity_zen_model" => self.antigravity_zen_model = val,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let cfg = CaCodeConfig::default();
        assert_eq!(cfg.antigravity_zen_api_key, None);
        assert_eq!(cfg.antigravity_zen_model, Some("deepseek-v4-flash-free".to_string()));
        assert_eq!(cfg.effective_antigravity_model(), "deepseek-v4-flash-free");
    }

    #[test]
    fn test_set_field_antigravity() {
        let mut cfg = CaCodeConfig::default();
        cfg.set_field("antigravity_zen_api_key", "ag-secret-key-12345".to_string());
        cfg.set_field("antigravity_zen_model", "minimax-m2.5-free".to_string());

        assert_eq!(cfg.antigravity_zen_api_key.as_deref(), Some("ag-secret-key-12345"));
        assert_eq!(cfg.antigravity_zen_model.as_deref(), Some("minimax-m2.5-free"));
        assert_eq!(cfg.effective_antigravity_api_key(), "ag-secret-key-12345");
        assert_eq!(cfg.effective_antigravity_model(), "minimax-m2.5-free");
    }

    #[test]
    fn test_display_fields_contains_antigravity() {
        let cfg = CaCodeConfig::default();
        let fields = cfg.display_fields();
        let zen_key_field = fields.iter().find(|(_, _, k, _)| *k == "antigravity_zen_api_key");
        assert!(zen_key_field.is_some());
        let zen_model_field = fields.iter().find(|(_, _, k, _)| *k == "antigravity_zen_model");
        assert!(zen_model_field.is_some());
        assert_eq!(zen_model_field.unwrap().3, "deepseek-v4-flash-free");
    }

    #[test]
    fn test_apply_to_env_antigravity() {
        let cfg = CaCodeConfig {
            antigravity_zen_api_key: Some("test-key-apply".to_string()),
            antigravity_zen_model: Some("mimo-v2.5-free".to_string()),
            ..Default::default()
        };
        cfg.apply_to_env();

        assert_eq!(env::var("ANTIGRAVITY_API_KEY").unwrap(), "test-key-apply");
        assert_eq!(env::var("ANTIGRAVITY_ZEN_API_KEY").unwrap(), "test-key-apply");
        assert_eq!(env::var("ANTIGRAVITY_ZEN_MODEL").unwrap(), "mimo-v2.5-free");

        env::remove_var("ANTIGRAVITY_API_KEY");
        env::remove_var("ANTIGRAVITY_ZEN_API_KEY");
        env::remove_var("ANTIGRAVITY_ZEN_MODEL");
    }

    #[test]
    fn test_custom_provider_config_serialization() {
        let mut cfg = CaCodeConfig::default();
        cfg.custom_providers.push(CustomProviderConfig {
            id: "groq-llama".to_string(),
            name: "Groq Llama 3".to_string(),
            base_url: "https://api.groq.com/openai/v1".to_string(),
            api_key: "gsk-mock-key".to_string(),
            model: "llama-3.3-70b-versatile".to_string(),
        });
        cfg.fallback_1 = Some("groq-llama".to_string());
        cfg.fallback_2 = Some("ollama".to_string());
        cfg.max_retries = 3;

        let toml_str = toml::to_string(&cfg).expect("failed to serialize toml");
        let parsed: CaCodeConfig = toml::from_str(&toml_str).expect("failed to deserialize toml");

        assert_eq!(parsed.custom_providers.len(), 1);
        assert_eq!(parsed.custom_providers[0].name, "Groq Llama 3");
        assert_eq!(parsed.fallback_1.as_deref(), Some("groq-llama"));
        assert_eq!(parsed.fallback_2.as_deref(), Some("ollama"));
        assert_eq!(parsed.max_retries, 3);
    }

    #[test]
    fn test_pro_license_detection() {
        let mut cfg = CaCodeConfig::default();
        assert!(!cfg.is_pro());

        cfg.pro_license_key = Some("   ".to_string());
        assert!(!cfg.is_pro());

        cfg.pro_license_key = Some("CACODE-PRO-KEY-2026".to_string());
        assert!(cfg.is_pro());
    }
}
