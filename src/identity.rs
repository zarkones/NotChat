//! App-layer Ed25519 identity (separate from Tor onion keys).

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use ed25519_dalek::{SigningKey, VerifyingKey};
use rand::rngs::OsRng;

/// Persistent identity: Ed25519 signing keypair + invite nonce for QR.
#[derive(Clone)]
pub struct Identity {
    pub signing: SigningKey,
    /// Current 16-byte invite nonce embedded in QR / validated on INTRO.
    pub invite_nonce: [u8; 16],
    /// Local display nick shared in intros.
    pub nick: String,
    /// 56-char onion hostname without `.onion` (filled once Tor publishes).
    pub onion_id: Option<String>,
}

impl Identity {
    pub fn generate(nick: impl Into<String>) -> Self {
        let signing = SigningKey::generate(&mut OsRng);
        let mut invite_nonce = [0u8; 16];
        rand::RngCore::fill_bytes(&mut OsRng, &mut invite_nonce);
        Self {
            signing,
            invite_nonce,
            nick: nick.into(),
            onion_id: None,
        }
    }

    pub fn verifying_key(&self) -> VerifyingKey {
        self.signing.verifying_key()
    }

    pub fn public_key_bytes(&self) -> [u8; 32] {
        self.verifying_key().to_bytes()
    }

    pub fn secret_key_bytes(&self) -> [u8; 32] {
        self.signing.to_bytes()
    }

    pub fn rotate_invite_nonce(&mut self) {
        rand::RngCore::fill_bytes(&mut OsRng, &mut self.invite_nonce);
    }

    /// Tor transport ID = onion hostname without `.onion`.
    pub fn set_onion_id_from_address(&mut self, onion_addr: &str) -> Result<()> {
        let id = onion_id_from_address(onion_addr)?;
        self.onion_id = Some(id);
        Ok(())
    }

    pub fn require_onion_id(&self) -> Result<&str> {
        self.onion_id
            .as_deref()
            .ok_or_else(|| anyhow!("onion id not yet known"))
    }
}

/// Strip `.onion` and validate 56-char v3 hostname.
pub fn onion_id_from_address(addr: &str) -> Result<String> {
    let trimmed = addr.trim().trim_end_matches('/').to_ascii_lowercase();
    let id = trimmed.strip_suffix(".onion").unwrap_or(&trimmed);
    validate_onion_id(id)?;
    Ok(id.to_string())
}

pub fn validate_onion_id(id: &str) -> Result<()> {
    if id.len() != 56 {
        return Err(anyhow!(
            "onion id must be 56 characters (got {})",
            id.len()
        ));
    }
    if !id
        .bytes()
        .all(|b| matches!(b, b'a'..=b'z' | b'2'..=b'7'))
    {
        return Err(anyhow!("onion id must be base32 (a-z, 2-7)"));
    }
    Ok(())
}

pub fn onion_address_from_id(id: &str) -> Result<String> {
    validate_onion_id(id)?;
    Ok(format!("{id}.onion"))
}

/// Default writable data root for Arti + SQLite.
pub fn default_data_root() -> PathBuf {
    let mut candidates: Vec<PathBuf> = Vec::new();

    #[cfg(target_os = "android")]
    {
        candidates.push(PathBuf::from(
            "/data/data/dev.zarkones.onion_chat/files/onion-chat",
        ));
        if let Ok(home) = std::env::var("HOME") {
            candidates.push(PathBuf::from(home).join("onion-chat"));
        }
        candidates.push(std::env::temp_dir().join("onion-chat"));
    }

    #[cfg(not(target_os = "android"))]
    {
        candidates.push(PathBuf::from("data"));
        if let Ok(home) = std::env::var("HOME") {
            candidates.push(PathBuf::from(home).join(".local/share/onion-chat"));
        }
        candidates.push(std::env::temp_dir().join("onion-chat"));
    }

    for c in candidates {
        if dir_is_writable(&c) {
            return c;
        }
    }
    std::env::temp_dir().join("onion-chat")
}

fn dir_is_writable(path: &Path) -> bool {
    if std::fs::create_dir_all(path).is_err() {
        return false;
    }
    let probe = path.join(".write_probe");
    match std::fs::write(&probe, b"ok") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

pub fn ensure_private_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)
        .with_context(|| format!("create dir {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path)
            .with_context(|| format!("stat {}", path.display()))?
            .permissions();
        perms.set_mode(0o700);
        std::fs::set_permissions(path, perms)
            .with_context(|| format!("chmod 0700 {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn onion_id_roundtrip() {
        let id = "px4yu6nwlmh35iy4nchzrgbklbslmtk4z2lpqbqqltpe2f3y6hllevyd";
        assert_eq!(onion_id_from_address(&format!("{id}.onion")).unwrap(), id);
        assert!(validate_onion_id(id).is_ok());
        assert!(validate_onion_id("short").is_err());
    }
}
