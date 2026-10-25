use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::path::PathBuf;

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
    pub thinking_mode: String,
    #[serde(default = "default_theme")]
    pub theme: String,
    #[serde(default = "default_app_name")]
    pub app_name: String,
    #[serde(default)]
    pub pro_license_key: Option<String>,
}

fn default_theme() -> String { "teal".into() }
fn default_app_name() -> String { "CACode — Sovereign Dev Agent".into() }

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
            thinking_mode: "Fast".into(),
            theme: default_theme(),
            app_name: default_app_name(),
            pro_license_key: None,
        }
    }
}

impl CaCodeConfig {
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

    pub fn display_fields(&self) -> Vec<(&'static str, &'static str, &'static str, String)> {
        vec![
            ("App Name", "CACODE_APP_NAME", "app_name", self.app_name.clone()),
            ("Theme Style", "CACODE_THEME", "theme", self.theme.clone()),
            ("Thinking Mode", "CACODE_THINKING_MODE", "thinking_mode", self.thinking_mode.clone()),
            ("Pro License Key", "CACODE_PRO_LICENSE", "pro_license_key", Self::mask(&self.pro_license_key.clone().unwrap_or_default())),
            ("OpenAI API Key", "OPENAI_API_KEY", "openai_api_key", Self::mask(&self.effective("OPENAI_API_KEY", &self.openai_api_key))),
            ("OpenAI Base URL", "OPENAI_BASE_URL", "openai_base_url", self.effective("OPENAI_BASE_URL", &self.openai_base_url)),
            ("Anthropic API Key", "ANTHROPIC_API_KEY", "anthropic_api_key", Self::mask(&self.effective("ANTHROPIC_API_KEY", &self.anthropic_api_key))),
            ("Ollama URL", "OLLAMA_URL", "ollama_url", self.effective("OLLAMA_URL", &self.ollama_url)),
            ("Ollama Model", "OLLAMA_MODEL", "ollama_model", self.effective("OLLAMA_MODEL", &self.ollama_model)),
            ("9Router URL", "ROUTER9_URL", "router9_url", self.effective("ROUTER9_URL", &self.router9_url)),
            ("9Router API Key", "ROUTER9_API_KEY", "router9_api_key", Self::mask(&self.effective("ROUTER9_API_KEY", &self.router9_api_key))),
            ("9Router Model", "ROUTER9_MODEL", "router9_model", self.effective("ROUTER9_MODEL", &self.router9_model)),
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
            "pro_license_key" => self.pro_license_key = val,
            "openai_api_key" => self.openai_api_key = val,
            "openai_base_url" => self.openai_base_url = val,
            "anthropic_api_key" => self.anthropic_api_key = val,
            "ollama_url" => self.ollama_url = val,
            "ollama_model" => self.ollama_model = val,
            "router9_url" => self.router9_url = val,
            "router9_api_key" => self.router9_api_key = val,
            "router9_model" => self.router9_model = val,
            _ => {}
        }
    }
}
