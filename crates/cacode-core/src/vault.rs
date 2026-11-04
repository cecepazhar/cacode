//! Local encrypted secret vault (`~/.cacode/secrets.enc`).
//!
//! ChaCha20-Poly1305 with a random 256-bit key in `~/.cacode/vault.key`
//! (mode 0600). This keeps API keys out of `config.toml`, so they do not
//! leak through screen-sharing, dotfile repos, backups or support bundles.
//! It does NOT protect against malware running as the same OS user (the key
//! file sits next to the vault); use OS disk encryption for that threat.

use anyhow::{anyhow, Result};
use base64::Engine;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub struct Vault {
    data_path: PathBuf,
    key_path: PathBuf,
}

#[cfg(unix)]
fn restrict(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}
#[cfg(not(unix))]
fn restrict(_path: &Path) {}

impl Vault {
    pub fn in_dir(dir: &Path) -> Self {
        Self { data_path: dir.join("secrets.enc"), key_path: dir.join("vault.key") }
    }

    fn key(&self, create: bool) -> Result<Option<[u8; 32]>> {
        if let Ok(bytes) = std::fs::read(&self.key_path) {
            let arr: [u8; 32] = bytes.as_slice().try_into().map_err(|_| anyhow!("vault.key is corrupt (expected 32 bytes)"))?;
            return Ok(Some(arr));
        }
        if !create {
            return Ok(None);
        }
        let mut k = [0u8; 32];
        getrandom::getrandom(&mut k).map_err(|e| anyhow!("OS RNG failed: {}", e))?;
        if let Some(parent) = self.key_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&self.key_path, k)?;
        restrict(&self.key_path);
        Ok(Some(k))
    }

    pub fn load(&self) -> Result<BTreeMap<String, String>> {
        let Ok(blob) = std::fs::read_to_string(&self.data_path) else { return Ok(BTreeMap::new()) };
        let Some(key) = self.key(false)? else { return Err(anyhow!("secrets.enc exists but vault.key is missing")) };
        let raw = base64::engine::general_purpose::STANDARD.decode(blob.trim())?;
        if raw.len() < 12 {
            return Err(anyhow!("secrets.enc is truncated"));
        }
        let (nonce, ct) = raw.split_at(12);
        let cipher = ChaCha20Poly1305::new(Key::from_slice(&key));
        let pt = cipher.decrypt(Nonce::from_slice(nonce), ct).map_err(|_| anyhow!("secrets.enc failed authentication (wrong key or tampered)"))?;
        Ok(serde_json::from_slice(&pt)?)
    }

    pub fn save(&self, secrets: &BTreeMap<String, String>) -> Result<()> {
        let key = self.key(true)?.expect("created");
        let cipher = ChaCha20Poly1305::new(Key::from_slice(&key));
        let mut nonce = [0u8; 12];
        getrandom::getrandom(&mut nonce).map_err(|e| anyhow!("OS RNG failed: {}", e))?;
        let ct = cipher.encrypt(Nonce::from_slice(&nonce), serde_json::to_vec(secrets)?.as_slice()).map_err(|_| anyhow!("encryption failed"))?;
        let mut out = nonce.to_vec();
        out.extend(ct);
        if let Some(parent) = self.data_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.data_path.with_extension("enc.tmp");
        std::fs::write(&tmp, base64::engine::general_purpose::STANDARD.encode(out))?;
        restrict(&tmp);
        std::fs::rename(tmp, &self.data_path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_tamper_detection() {
        let d = tempfile::tempdir().unwrap();
        let v = Vault::in_dir(d.path());
        let mut m = BTreeMap::new();
        m.insert("openai_api_key".to_string(), "sk-test-123".to_string());
        v.save(&m).unwrap();
        let raw = std::fs::read_to_string(d.path().join("secrets.enc")).unwrap();
        assert!(!raw.contains("sk-test"));
        assert_eq!(v.load().unwrap(), m);
        // tamper
        let mut bytes = base64::engine::general_purpose::STANDARD.decode(raw.trim()).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        std::fs::write(d.path().join("secrets.enc"), base64::engine::general_purpose::STANDARD.encode(bytes)).unwrap();
        assert!(v.load().is_err());
    }
}
