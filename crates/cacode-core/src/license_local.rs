//! Local license storage — offline-first entitlement.
//!
//! After activation the server returns an `expires_at` unix timestamp.
//! This module persists it locally so every subsequent `is_pro()` check
//! works without network.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct LocalLicense {
    pub key: String,
    pub product: String,
    pub tier: String,
    pub expires_at: i64,   // 0 means lifetime
}

fn license_path(app: &str) -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("no home directory"))?;
    Ok(home.join(format!(".local/share/{app}/license.json")))
}

pub fn save_license(app: &str, lic: &LocalLicense) -> Result<()> {
    let p = license_path(app)?;
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let data = serde_json::to_string_pretty(lic)?;
    std::fs::write(&p, data)?;
    Ok(())
}

pub fn load_license(app: &str) -> Result<Option<LocalLicense>> {
    let p = license_path(app)?;
    if !p.exists() {
        return Ok(None);
    }
    let txt = std::fs::read_to_string(p)?;
    Ok(Some(serde_json::from_str(&txt)?))
}

pub fn remove_license(app: &str) -> Result<()> {
    let p = license_path(app)?;
    if p.exists() {
        std::fs::remove_file(p)?;
    }
    Ok(())
}

/// Returns true when the license is valid right now.
/// Lifetime tier (99) and `expires_at == 0` never expire; others are checked against `now`.
pub fn is_license_valid(lic: &LocalLicense) -> bool {
    if lic.tier == "99" || lic.expires_at == 0 {
        return true;
    }
    let now = chrono::Utc::now().timestamp();
    now <= lic.expires_at
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lic(tier: &str, exp: i64) -> LocalLicense {
        LocalLicense { key: "k".into(), product: "CD".into(), tier: tier.into(), expires_at: exp }
    }

    #[test]
    fn lifetime_and_expiry_semantics() {
        assert!(is_license_valid(&lic("99", 1)));
        assert!(is_license_valid(&lic("01", 0)), "expires_at = 0 means lifetime");
        assert!(!is_license_valid(&lic("01", 1)), "expired in 1970");
        assert!(is_license_valid(&lic("01", chrono::Utc::now().timestamp() + 3600)));
    }
}
