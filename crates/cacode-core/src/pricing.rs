//! Single source of truth for model prices and context windows.
//!
//! Built-in prices are list prices in USD per 1M tokens as known at release
//! time and **must be verified** against the provider's pricing page; override
//! or extend them in `~/.cacode/pricing.toml`:
//!
//! ```toml
//! ["claude-sonnet-4"]
//! input_per_million = 3.0
//! output_per_million = 15.0
//! context_window = 200000
//! ```
//! Keys match by exact model id first, then by longest prefix.

use serde::Deserialize;
use std::collections::HashMap;
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, Deserialize, PartialEq)]
pub struct Pricing {
    pub input_per_million: f64,
    pub output_per_million: f64,
    #[serde(default)]
    pub context_window: Option<u64>,
}

const fn p(i: f64, o: f64, ctx: u64) -> Pricing {
    Pricing { input_per_million: i, output_per_million: o, context_window: Some(ctx) }
}

/// (model id prefix, pricing). Longest matching prefix wins.
const BUILTIN: &[(&str, Pricing)] = &[
    // Anthropic
    ("claude-opus-4", p(15.0, 75.0, 200_000)),
    ("claude-sonnet-4", p(3.0, 15.0, 200_000)),
    ("claude-3-7-sonnet", p(3.0, 15.0, 200_000)),
    ("claude-3-5-sonnet", p(3.0, 15.0, 200_000)),
    ("claude-3-5-haiku", p(0.80, 4.0, 200_000)),
    ("claude-haiku-4", p(1.0, 5.0, 200_000)),
    ("claude-3-opus", p(15.0, 75.0, 200_000)),
    // OpenAI
    ("gpt-4o-mini", p(0.15, 0.60, 128_000)),
    ("gpt-4o", p(2.50, 10.0, 128_000)),
    ("gpt-4.1-nano", p(0.10, 0.40, 1_047_576)),
    ("gpt-4.1-mini", p(0.40, 1.60, 1_047_576)),
    ("gpt-4.1", p(2.00, 8.00, 1_047_576)),
    ("o4-mini", p(1.10, 4.40, 200_000)),
    ("o3-mini", p(1.10, 4.40, 200_000)),
    ("o3", p(2.00, 8.00, 200_000)),
    // DeepSeek
    ("deepseek-chat", p(0.27, 1.10, 64_000)),
    ("deepseek-reasoner", p(0.55, 2.19, 64_000)),
    ("deepseek-coder", p(0.27, 1.10, 64_000)),
    // Free / local tiers
    ("deepseek-v4-flash-free", p(0.0, 0.0, 64_000)),
    ("mimo-v2.5-free", p(0.0, 0.0, 32_000)),
    ("minimax-m2.5-free", p(0.0, 0.0, 64_000)),
    ("llama3", p(0.0, 0.0, 8_192)),
    ("llama3.1", p(0.0, 0.0, 128_000)),
    ("qwen2.5-coder", p(0.0, 0.0, 32_768)),
    ("qwen3", p(0.0, 0.0, 32_768)),
];

pub const DEFAULT_CONTEXT_WINDOW: u64 = 32_000;

fn overrides() -> &'static HashMap<String, Pricing> {
    static O: OnceLock<HashMap<String, Pricing>> = OnceLock::new();
    O.get_or_init(|| {
        dirs::home_dir()
            .map(|h| h.join(".cacode").join("pricing.toml"))
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| toml::from_str(&t).ok())
            .unwrap_or_default()
    })
}

/// Strip vendor prefixes such as `openai/`, `anthropic/` (OpenRouter style)
/// and Ollama tags (`llama3:8b`).
pub fn normalize_model(model: &str) -> String {
    let m = model.trim().to_lowercase();
    let m = m.rsplit('/').next().unwrap_or(&m).to_string();
    m.split(':').next().unwrap_or(&m).to_string()
}

pub fn lookup_in(model: &str, table: &HashMap<String, Pricing>) -> Option<Pricing> {
    let m = normalize_model(model);
    if let Some(p) = table.get(&m) {
        return Some(*p);
    }
    let from_table = table.iter().filter(|(k, _)| m.starts_with(k.as_str())).max_by_key(|(k, _)| k.len()).map(|(_, v)| *v);
    from_table.or_else(|| BUILTIN.iter().filter(|(k, _)| m.starts_with(k)).max_by_key(|(k, _)| k.len()).map(|(_, v)| *v))
}

/// Pricing for a model; `None` when unknown (cost reported as unknown, not $0).
pub fn lookup(model: &str) -> Option<Pricing> {
    lookup_in(model, overrides())
}

/// Local providers are free regardless of model name.
pub fn cost_usd(provider_label: &str, model: &str, prompt: i64, completion: i64) -> Option<f64> {
    let pl = provider_label.to_lowercase();
    if pl.contains("ollama") || pl.contains("zen") {
        return Some(0.0);
    }
    lookup(model).map(|p| (prompt as f64 * p.input_per_million + completion as f64 * p.output_per_million) / 1_000_000.0)
}

/// Cost including prompt-cache discounts. Approximate multipliers of the input
/// price: Anthropic reads 0.1× / writes 1.25×; DeepSeek hits ≈0.26×; others 0.5×.
pub fn cost_usage_usd(provider_label: &str, model: &str, u: &crate::types::TokenUsage) -> Option<f64> {
    let pl = provider_label.to_lowercase();
    if pl.contains("ollama") || pl.contains("zen") {
        return Some(0.0);
    }
    let p = lookup(model)?;
    let m = normalize_model(model);
    let (read_f, write_f) = if m.starts_with("claude") { (0.1, 1.25) } else if m.starts_with("deepseek") { (0.26, 1.0) } else { (0.5, 1.0) };
    let plain = (u.prompt_tokens - u.cache_read_tokens - u.cache_write_tokens).max(0) as f64;
    let input = plain + u.cache_read_tokens as f64 * read_f + u.cache_write_tokens as f64 * write_f;
    Some((input * p.input_per_million + u.completion_tokens as f64 * p.output_per_million) / 1_000_000.0)
}

pub fn context_window(model: &str) -> u64 {
    lookup(model).and_then(|p| p.context_window).unwrap_or(DEFAULT_CONTEXT_WINDOW)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_model_ids_resolve() {
        let t = HashMap::new();
        assert_eq!(lookup_in("claude-sonnet-4-20250514", &t).unwrap().input_per_million, 3.0);
        assert_eq!(lookup_in("anthropic/claude-opus-4-1", &t).unwrap().output_per_million, 75.0);
        assert_eq!(lookup_in("gpt-4o-mini-2024-07-18", &t).unwrap().input_per_million, 0.15);
        assert_eq!(lookup_in("gpt-4o", &t).unwrap().input_per_million, 2.50);
        assert_eq!(lookup_in("deepseek-chat", &t).unwrap().input_per_million, 0.27);
        assert!(lookup_in("totally-unknown-model", &t).is_none());
        assert_eq!(cost_usd("Ollama", "anything", 1000, 1000), Some(0.0));
        let c = cost_usd("Anthropic", "claude-sonnet-4-20250514", 1_000_000, 1_000_000).unwrap();
        assert!((c - 18.0).abs() < 1e-9);
    }
}
