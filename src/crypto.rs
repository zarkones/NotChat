//! Signing, sealed boxes, skew + replay checks.

use anyhow::{anyhow, bail, Result};
use blake3::Hasher;
use crypto_box::aead::{Aead, AeadCore, Payload};
use crypto_box::{ChaChaBox, PublicKey as BoxPublicKey, SecretKey as BoxSecretKey};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rand::rngs::OsRng;
use sha2::{Digest, Sha512};

/// Reject frames whose timestamp differs from local clock by more than this.
pub const MAX_SKEW_MS: i64 = 10 * 60 * 1000; // 10 minutes

/// Canonical bytes: `ver || type || ts || body` (type NUL-terminated).
pub fn canonical_bytes(ver: u32, typ: &str, ts: i64, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + typ.len() + 1 + 8 + body.len());
    out.extend_from_slice(&ver.to_le_bytes());
    out.extend_from_slice(typ.as_bytes());
    out.push(0);
    out.extend_from_slice(&ts.to_le_bytes());
    out.extend_from_slice(body);
    out
}

/// blake3 digest of canonical bytes (handy for debugging / compact compare).
pub fn canonical_hash(ver: u32, typ: &str, ts: i64, body: &[u8]) -> [u8; 32] {
    let mut h = Hasher::new();
    h.update(&canonical_bytes(ver, typ, ts, body));
    *h.finalize().as_bytes()
}

pub fn sign(signing: &SigningKey, ver: u32, typ: &str, ts: i64, body: &[u8]) -> Signature {
    let msg = canonical_bytes(ver, typ, ts, body);
    signing.sign(&msg)
}

pub fn verify_sig(
    pk: &VerifyingKey,
    ver: u32,
    typ: &str,
    ts: i64,
    body: &[u8],
    sig: &Signature,
) -> Result<()> {
    let msg = canonical_bytes(ver, typ, ts, body);
    pk.verify(&msg, sig)
        .map_err(|_| anyhow!("bad signature"))?;
    Ok(())
}

pub fn now_unix_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn check_skew(ts: i64) -> Result<()> {
    let now = now_unix_ms();
    let delta = (now - ts).abs();
    if delta > MAX_SKEW_MS {
        bail!("timestamp skew {delta}ms exceeds {}ms", MAX_SKEW_MS);
    }
    Ok(())
}

/// Ed25519 secret → X25519 secret (libsodium `crypto_sign_ed25519_sk_to_curve25519`).
pub fn ed25519_sk_to_x25519(signing: &SigningKey) -> [u8; 32] {
    let hash = Sha512::digest(signing.to_bytes());
    let mut out = [0u8; 32];
    out.copy_from_slice(&hash[..32]);
    out
}

/// Ed25519 public → X25519 public (Edwards → Montgomery).
pub fn ed25519_pk_to_x25519(pk: &VerifyingKey) -> Result<[u8; 32]> {
    use curve25519_dalek::edwards::CompressedEdwardsY;
    let compressed = CompressedEdwardsY(pk.to_bytes());
    let point = compressed
        .decompress()
        .ok_or_else(|| anyhow!("invalid ed25519 public key (decompress)"))?;
    Ok(point.to_montgomery().to_bytes())
}

/// Seal UTF-8 plaintext to peer's identity pubkey (XChaCha20-Poly1305 crypto_box).
/// Wire ciphertext = nonce(24) || ciphertext+tag.
pub fn seal_message(
    sender_signing: &SigningKey,
    recipient_pk: &VerifyingKey,
    plaintext: &[u8],
) -> Result<Vec<u8>> {
    let sk = BoxSecretKey::from(ed25519_sk_to_x25519(sender_signing));
    let pk_bytes = ed25519_pk_to_x25519(recipient_pk)?;
    let pk = BoxPublicKey::from(pk_bytes);
    let boxed = ChaChaBox::new(&pk, &sk);
    let nonce = ChaChaBox::generate_nonce(&mut OsRng);
    let ct = boxed
        .encrypt(
            &nonce,
            Payload {
                msg: plaintext,
                aad: b"",
            },
        )
        .map_err(|_| anyhow!("seal encrypt failed"))?;
    let mut out = Vec::with_capacity(nonce.len() + ct.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// Open sealed message from peer's identity pubkey.
pub fn open_message(
    recipient_signing: &SigningKey,
    sender_pk: &VerifyingKey,
    wire: &[u8],
) -> Result<Vec<u8>> {
    if wire.len() < 24 {
        bail!("ciphertext too short");
    }
    let (nonce_bytes, ct) = wire.split_at(24);
    let nonce = crypto_box::Nonce::from_slice(nonce_bytes);
    let sk = BoxSecretKey::from(ed25519_sk_to_x25519(recipient_signing));
    let pk_bytes = ed25519_pk_to_x25519(sender_pk)?;
    let pk = BoxPublicKey::from(pk_bytes);
    let boxed = ChaChaBox::new(&pk, &sk);
    boxed
        .decrypt(
            nonce,
            Payload {
                msg: ct,
                aad: b"",
            },
        )
        .map_err(|_| anyhow!("seal decrypt failed"))
}

pub fn verifying_key_from_bytes(bytes: &[u8]) -> Result<VerifyingKey> {
    if bytes.len() != 32 {
        bail!("ed25519 pk must be 32 bytes");
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(bytes);
    VerifyingKey::from_bytes(&arr).map_err(|e| anyhow!("invalid ed25519 pk: {e}"))
}

pub fn signature_from_bytes(bytes: &[u8]) -> Result<Signature> {
    if bytes.len() != 64 {
        bail!("signature must be 64 bytes");
    }
    let mut arr = [0u8; 64];
    arr.copy_from_slice(bytes);
    Ok(Signature::from_bytes(&arr))
}

pub fn signing_key_from_bytes(bytes: &[u8]) -> Result<SigningKey> {
    if bytes.len() != 32 {
        bail!("ed25519 sk must be 32 bytes");
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(bytes);
    Ok(SigningKey::from_bytes(&arr))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;

    #[test]
    fn sign_verify_roundtrip() {
        let sk = SigningKey::generate(&mut OsRng);
        let body = b"hello-body";
        let ts = now_unix_ms();
        let sig = sign(&sk, 1, "intro", ts, body);
        verify_sig(&sk.verifying_key(), 1, "intro", ts, body, &sig).unwrap();
        assert!(verify_sig(&sk.verifying_key(), 1, "intro", ts, b"other", &sig).is_err());
    }

    #[test]
    fn seal_open_roundtrip() {
        let a = SigningKey::generate(&mut OsRng);
        let b = SigningKey::generate(&mut OsRng);
        let wire = seal_message(&a, &b.verifying_key(), b"secret text").unwrap();
        let plain = open_message(&b, &a.verifying_key(), &wire).unwrap();
        assert_eq!(plain, b"secret text");
    }

    #[test]
    fn skew_rejects_old() {
        let old = now_unix_ms() - MAX_SKEW_MS - 1000;
        assert!(check_skew(old).is_err());
        assert!(check_skew(now_unix_ms()).is_ok());
    }
}
