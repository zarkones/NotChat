//! Frozen wire format: QR URI + HTTP JSON frames (SPEC-v0).

use anyhow::{anyhow, bail, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ed25519_dalek::{Signature, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::crypto::{
    check_skew, open_message, seal_message, sign, signature_from_bytes, verify_sig,
    verifying_key_from_bytes,
};
use crate::identity::{validate_onion_id, Identity};

pub const WIRE_VER: u32 = 1;

// ---------- encoding helpers ----------

pub fn b64url_encode(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

pub fn b64url_decode(s: &str) -> Result<Vec<u8>> {
    URL_SAFE_NO_PAD
        .decode(s.trim())
        .map_err(|e| anyhow!("base64url decode: {e}"))
}

// ---------- QR / manual ID ----------

/// Parsed `onionchat:v1?...` invite (or bare 56-char id → TOFU).
#[derive(Debug, Clone)]
pub struct Invite {
    pub id: String,
    pub pk: Option<[u8; 32]>,
    pub nonce: Option<[u8; 16]>,
    pub nick: Option<String>,
    /// True when only the 56-char id was pasted (no pk yet).
    pub tofu: bool,
}

impl Invite {
    pub fn encode_uri(id: &str, pk: &[u8; 32], nonce: &[u8; 16], nick: &str) -> String {
        let mut uri = format!(
            "onionchat:v1?id={}&pk={}&n={}",
            id,
            b64url_encode(pk),
            b64url_encode(nonce)
        );
        if !nick.is_empty() {
            uri.push_str("&nick=");
            uri.push_str(&urlencoding_encode(nick));
        }
        uri
    }

    pub fn from_identity(identity: &Identity) -> Result<String> {
        let id = identity.require_onion_id()?;
        Ok(Self::encode_uri(
            id,
            &identity.public_key_bytes(),
            &identity.invite_nonce,
            &identity.nick,
        ))
    }

    pub fn parse(input: &str) -> Result<Self> {
        let input = input.trim();
        if input.is_empty() {
            bail!("empty invite");
        }

        // Bare 56-char id → TOFU
        if !input.contains(':') && !input.contains('?') && !input.contains('.') {
            validate_onion_id(input)?;
            return Ok(Self {
                id: input.to_ascii_lowercase(),
                pk: None,
                nonce: None,
                nick: None,
                tofu: true,
            });
        }

        // Allow accidental .onion paste of full address without scheme
        if input.ends_with(".onion") && !input.contains('?') {
            let id = crate::identity::onion_id_from_address(input)?;
            return Ok(Self {
                id,
                pk: None,
                nonce: None,
                nick: None,
                tofu: true,
            });
        }

        let url = if input.starts_with("onionchat:") {
            // url crate wants a proper scheme; treat as onionchat://v1?...
            let rest = input.trim_start_matches("onionchat:");
            let normalized = if rest.starts_with("//") {
                format!("onionchat:{rest}")
            } else if rest.starts_with('v') {
                format!("onionchat://{rest}")
            } else {
                format!("onionchat://{rest}")
            };
            url::Url::parse(&normalized).context("parse onionchat URI")?
        } else {
            bail!("expected onionchat:v1?... URI or 56-char id");
        };

        if url.scheme() != "onionchat" {
            bail!("unsupported scheme");
        }

        // Require v1: onionchat:v1?… normalizes to host "v1".
        let host = url.host_str().unwrap_or("").trim_matches('/');
        let path_ver = url.path().trim_matches('/');
        let is_v1 = host.eq_ignore_ascii_case("v1")
            || path_ver.eq_ignore_ascii_case("v1")
            || (host.is_empty() && path_ver.is_empty() && input.trim().starts_with("onionchat:v1"));
        if !is_v1 {
            bail!("only onionchat:v1 invites are accepted");
        }

        let mut id = None::<String>;
        let mut pk = None::<[u8; 32]>;
        let mut nonce = None::<[u8; 16]>;
        let mut nick = None::<String>;

        for (k, v) in url.query_pairs() {
            match k.as_ref() {
                "id" => {
                    validate_onion_id(&v)?;
                    id = Some(v.to_ascii_lowercase());
                }
                "pk" => {
                    let raw = b64url_decode(&v)?;
                    if raw.len() != 32 {
                        bail!("pk must decode to 32 bytes");
                    }
                    let mut arr = [0u8; 32];
                    arr.copy_from_slice(&raw);
                    // validate curve point
                    let _ = verifying_key_from_bytes(&arr)?;
                    pk = Some(arr);
                }
                "n" | "nonce" => {
                    let raw = b64url_decode(&v)?;
                    if raw.len() != 16 {
                        bail!("nonce must decode to 16 bytes");
                    }
                    let mut arr = [0u8; 16];
                    arr.copy_from_slice(&raw);
                    nonce = Some(arr);
                }
                "nick" => nick = Some(v.to_string()),
                "ver" => {
                    if v.as_ref() != "1" {
                        bail!("unsupported invite ver");
                    }
                }
                _ => {}
            }
        }

        let id = id.ok_or_else(|| anyhow!("missing id"))?;
        let tofu = pk.is_none();
        Ok(Self {
            id,
            pk,
            nonce,
            nick,
            tofu,
        })
    }

    /// Parse a camera/file QR payload: only full `onionchat:v1` with pk + nonce.
    /// Rejects bare IDs, TOFU stubs, and garbage.
    pub fn parse_scanned(input: &str) -> Result<Self> {
        let input = input.trim();
        if input.is_empty() {
            bail!("empty QR payload");
        }
        // Hard reject non-invite schemes / junk before soft parse.
        let lower = input.to_ascii_lowercase();
        if !(lower.starts_with("onionchat:v1") || lower.starts_with("onionchat://v1")) {
            bail!("QR must encode onionchat:v1?… (rejected)");
        }
        let inv = Self::parse(input)?;
        if inv.tofu || inv.pk.is_none() || inv.nonce.is_none() {
            bail!("QR invite incomplete — need id, pk, and nonce");
        }
        Ok(inv)
    }
}

fn urlencoding_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ---------- Frame bodies for signing ----------

pub fn intro_sign_body(
    from_id: &str,
    from_pk: &[u8; 32],
    from_nick: &str,
    to_nonce: &[u8; 16],
) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(from_id.as_bytes());
    b.push(0);
    b.extend_from_slice(from_pk);
    b.extend_from_slice(from_nick.as_bytes());
    b.push(0);
    b.extend_from_slice(to_nonce);
    b
}

pub fn intro_ack_sign_body(from_id: &str, from_pk: &[u8; 32], decision: &str) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(from_id.as_bytes());
    b.push(0);
    b.extend_from_slice(from_pk);
    b.extend_from_slice(decision.as_bytes());
    b.push(0);
    b
}

pub fn msg_sign_body(from_id: &str, msg_id: &str, ciphertext: &[u8]) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(from_id.as_bytes());
    b.push(0);
    b.extend_from_slice(msg_id.as_bytes());
    b.push(0);
    b.extend_from_slice(ciphertext);
    b
}

// ---------- JSON frames ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntroFrame {
    pub ver: u32,
    #[serde(rename = "type")]
    pub typ: String,
    pub ts: i64,
    pub from_id: String,
    pub from_pk: String,
    pub from_nick: String,
    pub to_nonce: String,
    pub sig: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntroAckFrame {
    pub ver: u32,
    #[serde(rename = "type")]
    pub typ: String,
    pub ts: i64,
    pub from_id: String,
    pub from_pk: String,
    pub decision: String,
    pub sig: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MsgFrame {
    pub ver: u32,
    #[serde(rename = "type")]
    pub typ: String,
    pub ts: i64,
    pub from_id: String,
    pub msg_id: String,
    pub ciphertext: String,
    pub sig: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthResponse {
    pub ok: bool,
    pub ver: u32,
}

impl IntroFrame {
    pub fn build(
        identity: &Identity,
        to_nonce: &[u8; 16],
        ts: i64,
    ) -> Result<Self> {
        let from_id = identity.require_onion_id()?.to_string();
        let from_pk = identity.public_key_bytes();
        let body = intro_sign_body(&from_id, &from_pk, &identity.nick, to_nonce);
        let sig = sign(&identity.signing, WIRE_VER, "intro", ts, &body);
        Ok(Self {
            ver: WIRE_VER,
            typ: "intro".into(),
            ts,
            from_id,
            from_pk: b64url_encode(&from_pk),
            from_nick: identity.nick.clone(),
            to_nonce: b64url_encode(to_nonce),
            sig: b64url_encode(&sig.to_bytes()),
        })
    }

    pub fn verify_and_parse(&self) -> Result<VerifiedIntro> {
        if self.ver != WIRE_VER || self.typ != "intro" {
            bail!("bad intro envelope");
        }
        check_skew(self.ts)?;
        validate_onion_id(&self.from_id)?;
        let pk_raw = b64url_decode(&self.from_pk)?;
        let pk = verifying_key_from_bytes(&pk_raw)?;
        let mut pk_arr = [0u8; 32];
        pk_arr.copy_from_slice(&pk_raw);
        let nonce_raw = b64url_decode(&self.to_nonce)?;
        if nonce_raw.len() != 16 {
            bail!("bad to_nonce length");
        }
        let mut nonce = [0u8; 16];
        nonce.copy_from_slice(&nonce_raw);
        let sig_raw = b64url_decode(&self.sig)?;
        let sig = signature_from_bytes(&sig_raw)?;
        let body = intro_sign_body(&self.from_id, &pk_arr, &self.from_nick, &nonce);
        verify_sig(&pk, self.ver, "intro", self.ts, &body, &sig)?;
        Ok(VerifiedIntro {
            from_id: self.from_id.clone(),
            from_pk: pk_arr,
            from_pk_v: pk,
            from_nick: self.from_nick.clone(),
            to_nonce: nonce,
            ts: self.ts,
        })
    }
}

#[derive(Debug, Clone)]
pub struct VerifiedIntro {
    pub from_id: String,
    pub from_pk: [u8; 32],
    pub from_pk_v: VerifyingKey,
    pub from_nick: String,
    pub to_nonce: [u8; 16],
    pub ts: i64,
}

impl IntroAckFrame {
    pub fn build(identity: &Identity, decision: &str, ts: i64) -> Result<Self> {
        if decision != "accept" && decision != "reject" {
            bail!("decision must be accept|reject");
        }
        let from_id = identity.require_onion_id()?.to_string();
        let from_pk = identity.public_key_bytes();
        let body = intro_ack_sign_body(&from_id, &from_pk, decision);
        let sig = sign(&identity.signing, WIRE_VER, "intro_ack", ts, &body);
        Ok(Self {
            ver: WIRE_VER,
            typ: "intro_ack".into(),
            ts,
            from_id,
            from_pk: b64url_encode(&from_pk),
            decision: decision.into(),
            sig: b64url_encode(&sig.to_bytes()),
        })
    }

    pub fn verify_and_parse(&self) -> Result<VerifiedAck> {
        if self.ver != WIRE_VER || self.typ != "intro_ack" {
            bail!("bad intro_ack envelope");
        }
        check_skew(self.ts)?;
        validate_onion_id(&self.from_id)?;
        if self.decision != "accept" && self.decision != "reject" {
            bail!("bad decision");
        }
        let pk_raw = b64url_decode(&self.from_pk)?;
        let pk = verifying_key_from_bytes(&pk_raw)?;
        let mut pk_arr = [0u8; 32];
        pk_arr.copy_from_slice(&pk_raw);
        let sig = signature_from_bytes(&b64url_decode(&self.sig)?)?;
        let body = intro_ack_sign_body(&self.from_id, &pk_arr, &self.decision);
        verify_sig(&pk, self.ver, "intro_ack", self.ts, &body, &sig)?;
        Ok(VerifiedAck {
            from_id: self.from_id.clone(),
            from_pk: pk_arr,
            from_pk_v: pk,
            decision: self.decision.clone(),
            ts: self.ts,
        })
    }
}

#[derive(Debug, Clone)]
pub struct VerifiedAck {
    pub from_id: String,
    pub from_pk: [u8; 32],
    pub from_pk_v: VerifyingKey,
    pub decision: String,
    pub ts: i64,
}

impl MsgFrame {
    pub fn build(
        identity: &Identity,
        recipient_pk: &VerifyingKey,
        msg_id: &str,
        plaintext: &str,
        ts: i64,
    ) -> Result<Self> {
        let from_id = identity.require_onion_id()?.to_string();
        let ct = seal_message(&identity.signing, recipient_pk, plaintext.as_bytes())?;
        let body = msg_sign_body(&from_id, msg_id, &ct);
        let sig = sign(&identity.signing, WIRE_VER, "msg", ts, &body);
        Ok(Self {
            ver: WIRE_VER,
            typ: "msg".into(),
            ts,
            from_id,
            msg_id: msg_id.to_string(),
            ciphertext: b64url_encode(&ct),
            sig: b64url_encode(&sig.to_bytes()),
        })
    }

    /// Verify signature + skew; caller supplies sender pk (from contacts DB).
    pub fn verify_sig_only(&self, sender_pk: &VerifyingKey) -> Result<Vec<u8>> {
        if self.ver != WIRE_VER || self.typ != "msg" {
            bail!("bad msg envelope");
        }
        check_skew(self.ts)?;
        validate_onion_id(&self.from_id)?;
        let ct = b64url_decode(&self.ciphertext)?;
        let sig = signature_from_bytes(&b64url_decode(&self.sig)?)?;
        let body = msg_sign_body(&self.from_id, &self.msg_id, &ct);
        verify_sig(sender_pk, self.ver, "msg", self.ts, &body, &sig)?;
        Ok(ct)
    }

    pub fn decrypt(
        &self,
        recipient: &SigningKey,
        sender_pk: &VerifyingKey,
        ciphertext: &[u8],
    ) -> Result<String> {
        let plain = open_message(recipient, sender_pk, ciphertext)?;
        String::from_utf8(plain).context("message not utf-8")
    }
}

/// Encode a Signature for tests / debug.
pub fn sig_bytes(sig: &Signature) -> [u8; 64] {
    sig.to_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::now_unix_ms;
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;

    #[test]
    fn qr_roundtrip() {
        let sk = SigningKey::generate(&mut OsRng);
        let pk = sk.verifying_key().to_bytes();
        let nonce = [7u8; 16];
        let uri = Invite::encode_uri(
            "px4yu6nwlmh35iy4nchzrgbklbslmtk4z2lpqbqqltpe2f3y6hllevyd",
            &pk,
            &nonce,
            "Ada",
        );
        let inv = Invite::parse(&uri).unwrap();
        assert!(!inv.tofu);
        assert_eq!(inv.nick.as_deref(), Some("Ada"));
        assert_eq!(inv.pk.unwrap(), pk);
        assert_eq!(inv.nonce.unwrap(), nonce);
    }

    #[test]
    fn bare_id_is_tofu() {
        let id = "px4yu6nwlmh35iy4nchzrgbklbslmtk4z2lpqbqqltpe2f3y6hllevyd";
        let inv = Invite::parse(id).unwrap();
        assert!(inv.tofu);
    }

    #[test]
    fn reject_wrong_scheme_and_ver() {
        assert!(Invite::parse("https://evil.example/x").is_err());
        assert!(Invite::parse("onionchat:v2?id=px4yu6nwlmh35iy4nchzrgbklbslmtk4z2lpqbqqltpe2f3y6hllevyd").is_err());
        assert!(Invite::parse_scanned("not-a-qr").is_err());
        assert!(Invite::parse_scanned("px4yu6nwlmh35iy4nchzrgbklbslmtk4z2lpqbqqltpe2f3y6hllevyd").is_err());
    }

    #[test]
    fn parse_scanned_accepts_full_uri() {
        let sk = SigningKey::generate(&mut OsRng);
        let pk = sk.verifying_key().to_bytes();
        let nonce = [9u8; 16];
        let uri = Invite::encode_uri(
            "px4yu6nwlmh35iy4nchzrgbklbslmtk4z2lpqbqqltpe2f3y6hllevyd",
            &pk,
            &nonce,
            "Ada",
        );
        let inv = Invite::parse_scanned(&uri).unwrap();
        assert!(!inv.tofu);
        assert_eq!(inv.pk.unwrap(), pk);
    }

    #[test]
    fn intro_sign_verify() {
        let mut id = Identity::generate("bob");
        id.onion_id = Some("px4yu6nwlmh35iy4nchzrgbklbslmtk4z2lpqbqqltpe2f3y6hllevyd".into());
        let nonce = [1u8; 16];
        let frame = IntroFrame::build(&id, &nonce, now_unix_ms()).unwrap();
        let v = frame.verify_and_parse().unwrap();
        assert_eq!(v.from_nick, "bob");
    }
}
