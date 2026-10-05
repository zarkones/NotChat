//! Minimal HTTP/1.1 on onion streams + v1 route handlers.

use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Context, Result};
use tracing::{info, warn};

use crate::crypto::{now_unix_ms, verifying_key_from_bytes};
use crate::db::{Contact, ContactStatus, Db, Message};
use crate::identity::Identity;
use crate::protocol::{
    HealthResponse, IntroAckFrame, IntroFrame, MsgFrame, WIRE_VER,
};

/// Shared mutable state for inbound HTTP + outbox.
pub struct AppShared {
    pub db: Mutex<Db>,
    pub identity: Mutex<Identity>,
}

impl AppShared {
    pub fn new(db: Db, identity: Identity) -> Arc<Self> {
        Arc::new(Self {
            db: Mutex::new(db),
            identity: Mutex::new(identity),
        })
    }
}

#[derive(Debug)]
pub struct HttpRequest {
    pub method: String,
    pub path: String,
    pub body: Vec<u8>,
}

/// Read one HTTP request (headers + body by Content-Length).
pub async fn read_http_request<S>(stream: &mut S) -> Result<HttpRequest>
where
    S: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;

    let mut buf = vec![0u8; 2048];
    let mut collected = Vec::new();
    let header_end;

    loop {
        let n = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            stream.read(&mut buf),
        )
        .await
        .map_err(|_| anyhow!("read headers timeout"))?
        .context("read headers")?;
        if n == 0 {
            bail!("connection closed before headers");
        }
        collected.extend_from_slice(&buf[..n]);
        if let Some(pos) = find_header_end(&collected) {
            header_end = pos;
            break;
        }
        if collected.len() > 64 * 1024 {
            bail!("headers too large");
        }
    }

    let header_bytes = &collected[..header_end];
    let header_str = std::str::from_utf8(header_bytes).context("headers not utf-8")?;
    let mut lines = header_str.split("\r\n");
    let request_line = lines.next().ok_or_else(|| anyhow!("empty request"))?;
    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| anyhow!("no method"))?
        .to_string();
    let path = parts.next().ok_or_else(|| anyhow!("no path"))?.to_string();

    let mut content_length = 0usize;
    for line in lines {
        let lower = line.to_ascii_lowercase();
        if let Some(v) = lower.strip_prefix("content-length:") {
            content_length = v.trim().parse().unwrap_or(0);
        }
    }

    if content_length > 1024 * 1024 {
        bail!("body too large");
    }

    let mut body = collected[header_end..].to_vec();
    while body.len() < content_length {
        let n = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            stream.read(&mut buf),
        )
        .await
        .map_err(|_| anyhow!("read body timeout"))?
        .context("read body")?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&buf[..n]);
    }
    body.truncate(content_length);

    Ok(HttpRequest { method, path, body })
}

fn find_header_end(data: &[u8]) -> Option<usize> {
    data.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

pub async fn write_http_response<S>(
    stream: &mut S,
    status: u16,
    reason: &str,
    content_type: &str,
    body: &[u8],
) -> Result<()>
where
    S: tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt;
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n",
        body.len()
    );
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.shutdown().await.ok();
    Ok(())
}

pub async fn write_json<S>(stream: &mut S, status: u16, reason: &str, value: &impl serde::Serialize) -> Result<()>
where
    S: tokio::io::AsyncWrite + Unpin,
{
    let body = serde_json::to_vec(value)?;
    write_http_response(stream, status, reason, "application/json", &body).await
}

pub async fn write_error<S>(stream: &mut S, status: u16, reason: &str, msg: &str) -> Result<()>
where
    S: tokio::io::AsyncWrite + Unpin,
{
    let body = serde_json::json!({ "ok": false, "error": msg });
    write_json(stream, status, reason, &body).await
}

/// Dispatch `/v1/*` routes. Returns true if handled.
pub async fn handle_api<S>(stream: &mut S, req: &HttpRequest, shared: &AppShared) -> Result<()>
where
    S: tokio::io::AsyncWrite + Unpin,
{
    let path = req.path.split('?').next().unwrap_or(&req.path);

    match (req.method.as_str(), path) {
        ("GET", "/v1/health") | ("GET", "/health") => {
            let resp = HealthResponse {
                ok: true,
                ver: WIRE_VER,
            };
            write_json(stream, 200, "OK", &resp).await
        }
        ("POST", "/v1/intro") => handle_intro(stream, req, shared).await,
        ("POST", "/v1/intro/ack") => handle_intro_ack(stream, req, shared).await,
        ("POST", "/v1/msg") => handle_msg(stream, req, shared).await,
        ("GET", "/") => {
            let body = b"NotChat v0 - use /v1/health\n";
            write_http_response(stream, 200, "OK", "text/plain; charset=utf-8", body).await
        }
        _ => write_error(stream, 404, "Not Found", "unknown path").await,
    }
}

async fn handle_intro<S>(stream: &mut S, req: &HttpRequest, shared: &AppShared) -> Result<()>
where
    S: tokio::io::AsyncWrite + Unpin,
{
    let frame: IntroFrame = match serde_json::from_slice(&req.body) {
        Ok(f) => f,
        Err(e) => return write_error(stream, 400, "Bad Request", &format!("json: {e}")).await,
    };

    let verified = match frame.verify_and_parse() {
        Ok(v) => v,
        Err(e) => {
            warn!(?e, "intro rejected");
            return write_error(stream, 401, "Unauthorized", &format!("{e}")).await;
        }
    };

    #[derive(Clone)]
    enum Out { Ok, Err(u16, &'static str, String) }

    // (peer_id, nick) for a "contact request" notification, posted after the
    // DB/identity locks are released.
    let mut notice: Option<(String, String)> = None;

    let outcome = (|| -> Out {
        let mut db = match shared.db.lock() {
            Ok(g) => g,
            Err(_) => return Out::Err(500, "Error", "db lock".into()),
        };
        let identity = match shared.identity.lock() {
            Ok(g) => g,
            Err(_) => return Out::Err(500, "Error", "id lock".into()),
        };

        if verified.to_nonce != identity.invite_nonce {
            warn!("intro nonce mismatch");
            return Out::Err(403, "Forbidden", "nonce mismatch".into());
        }

        let replay_key = format!(
            "intro:{}:{}",
            verified.from_id,
            hex::encode(verified.to_nonce)
        );
        if db.has_seen(&replay_key).unwrap_or(false) {
            return Out::Err(409, "Conflict", "replay".into());
        }
        let _ = db.mark_seen(&replay_key);

        if let Ok(Some(c)) = db.get_contact(&verified.from_id) {
            if c.status == ContactStatus::Accepted {
                return Out::Ok;
            }
        }

        let inserted = db
            .insert_contact_request(
                &verified.from_id,
                &verified.from_pk,
                &verified.from_nick,
                &verified.to_nonce,
                verified.ts,
            )
            .unwrap_or(false);
        let now = now_unix_ms();
        if db.get_contact(&verified.from_id).ok().flatten().is_none() {
            let _ = db.upsert_contact(&Contact {
                id: verified.from_id.clone(),
                pk: verified.from_pk,
                self_nick: verified.from_nick.clone(),
                custom_nick: None,
                status: ContactStatus::Pending,
                created_at: now,
                updated_at: now,
            });
        } else {
            let _ = db.update_contact_self_nick(&verified.from_id, &verified.from_nick);
        }
        info!(from = %verified.from_id, inserted, "contact request received");
        if inserted {
            notice = Some((verified.from_id.clone(), verified.from_nick.clone()));
        }
        Out::Ok
    })();

    if let Some((peer, nick)) = notice {
        crate::notify::on_contact_request(&peer, &nick);
    }

    match outcome {
        Out::Ok => write_json(stream, 200, "OK", &serde_json::json!({"ok": true})).await,
        Out::Err(code, reason, msg) => write_error(stream, code, reason, &msg).await,
    }
}

async fn handle_intro_ack<S>(stream: &mut S, req: &HttpRequest, shared: &AppShared) -> Result<()>
where
    S: tokio::io::AsyncWrite + Unpin,
{
    let frame: IntroAckFrame = match serde_json::from_slice(&req.body) {
        Ok(f) => f,
        Err(e) => return write_error(stream, 400, "Bad Request", &format!("json: {e}")).await,
    };

    let verified = match frame.verify_and_parse() {
        Ok(v) => v,
        Err(e) => return write_error(stream, 401, "Unauthorized", &format!("{e}")).await,
    };

    #[derive(Clone)]
    enum Out { Ok, Err(u16, &'static str, String) }

    let outcome = (|| -> Out {
        let mut db = match shared.db.lock() {
            Ok(g) => g,
            Err(_) => return Out::Err(500, "Error", "db lock".into()),
        };
        let replay_key = format!(
            "ack:{}:{}:{}",
            verified.from_id, verified.decision, verified.ts
        );
        if db.has_seen(&replay_key).unwrap_or(false) {
            return Out::Err(409, "Conflict", "replay".into());
        }
        let _ = db.mark_seen(&replay_key);
        let now = now_unix_ms();
        match verified.decision.as_str() {
            "accept" => {
                if let Ok(Some(mut c)) = db.get_contact(&verified.from_id) {
                    if c.pk != verified.from_pk {
                        warn!("ack pk mismatch for {}", verified.from_id);
                        return Out::Err(403, "Forbidden", "pk mismatch".into());
                    }
                    c.status = ContactStatus::Accepted;
                    c.updated_at = now;
                    let _ = db.upsert_contact(&c);
                } else {
                    let _ = db.upsert_contact(&Contact {
                        id: verified.from_id.clone(),
                        pk: verified.from_pk,
                        self_nick: String::new(),
                        custom_nick: None,
                        status: ContactStatus::Accepted,
                        created_at: now,
                        updated_at: now,
                    });
                }
                info!(from = %verified.from_id, "intro accepted by peer");
            }
            "reject" => {
                if db.get_contact(&verified.from_id).ok().flatten().is_some() {
                    let _ = db.set_contact_status(&verified.from_id, ContactStatus::Rejected);
                }
                info!(from = %verified.from_id, "intro rejected by peer");
            }
            _ => {}
        }
        Out::Ok
    })();

    match outcome {
        Out::Ok => write_json(stream, 200, "OK", &serde_json::json!({"ok": true})).await,
        Out::Err(code, reason, msg) => write_error(stream, code, reason, &msg).await,
    }
}

async fn handle_msg<S>(stream: &mut S, req: &HttpRequest, shared: &AppShared) -> Result<()>
where
    S: tokio::io::AsyncWrite + Unpin,
{
    let frame: MsgFrame = match serde_json::from_slice(&req.body) {
        Ok(f) => f,
        Err(e) => return write_error(stream, 400, "Bad Request", &format!("json: {e}")).await,
    };

    #[derive(Clone)]
    enum Out { Ok, Err(u16, &'static str, String) }

    // (peer_id, display name, plaintext) for the push notification; posted
    // only after the locks are released and only for a *new* message
    // (replays return 409 before reaching this point).
    let mut notice: Option<(String, String, String)> = None;

    let outcome = (|| -> Out {
        let mut db = match shared.db.lock() {
            Ok(g) => g,
            Err(_) => return Out::Err(500, "Error", "db lock".into()),
        };
        let identity = match shared.identity.lock() {
            Ok(g) => g,
            Err(_) => return Out::Err(500, "Error", "id lock".into()),
        };

        let contact = match db.get_contact(&frame.from_id) {
            Ok(Some(c)) if c.status == ContactStatus::Accepted => c,
            Ok(Some(_)) => return Out::Err(403, "Forbidden", "contact not accepted".into()),
            Ok(None) => return Out::Err(403, "Forbidden", "unknown contact".into()),
            Err(e) => return Out::Err(500, "Error", format!("{e}")),
        };

        let sender_pk = match verifying_key_from_bytes(&contact.pk) {
            Ok(pk) => pk,
            Err(e) => return Out::Err(400, "Bad Request", format!("{e}")),
        };
        let ct = match frame.verify_sig_only(&sender_pk) {
            Ok(ct) => ct,
            Err(e) => return Out::Err(401, "Unauthorized", format!("{e}")),
        };
        let replay_key = format!("msg:{}", frame.msg_id);
        if db.has_seen(&replay_key).unwrap_or(false) {
            return Out::Err(409, "Conflict", "replay".into());
        }
        match frame.decrypt(&identity.signing, &sender_pk, &ct) {
            Ok(plaintext) => {
                let _ = db.mark_seen(&replay_key);
                let _ = db.insert_message(&Message {
                    msg_id: frame.msg_id.clone(),
                    peer_id: frame.from_id.clone(),
                    direction: "in".into(),
                    plaintext: plaintext.clone(),
                    ts: frame.ts,
                    status: "received".into(),
                });
                info!(from = %frame.from_id, msg_id = %frame.msg_id, "message received");
                notice = Some((frame.from_id.clone(), contact.display_name(), plaintext));
                Out::Ok
            }
            Err(e) => Out::Err(400, "Bad Request", format!("decrypt: {e}")),
        }
    })();

    if let Some((peer, name, text)) = notice {
        // Skips itself when that peer's chat is open & the app is in front.
        crate::notify::on_incoming_message(&peer, &name, &text);
    }

    match outcome {
        Out::Ok => write_json(stream, 200, "OK", &serde_json::json!({"ok": true})).await,
        Out::Err(code, reason, msg) => write_error(stream, code, reason, &msg).await,
    }
}

/// Build a raw HTTP POST request body (headers + body).
pub fn build_http_post(path: &str, host: &str, json_body: &[u8]) -> Vec<u8> {
    let header = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: {host}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n",
        json_body.len()
    );
    let mut out = header.into_bytes();
    out.extend_from_slice(json_body);
    out
}

pub fn build_http_get(path: &str, host: &str) -> Vec<u8> {
    format!(
        "GET {path} HTTP/1.1\r\n\
         Host: {host}\r\n\
         Connection: close\r\n\
         \r\n"
    )
    .into_bytes()
}

/// Read status line from an HTTP response (best-effort).
pub async fn read_http_status<S>(stream: &mut S) -> Result<(u16, Vec<u8>)>
where
    S: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let mut buf = vec![0u8; 4096];
    let mut collected = Vec::new();
    loop {
        let n = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            stream.read(&mut buf),
        )
        .await
        .map_err(|_| anyhow!("response timeout"))?
        .context("read response")?;
        if n == 0 {
            break;
        }
        collected.extend_from_slice(&buf[..n]);
        if find_header_end(&collected).is_some() || collected.len() > 256 * 1024 {
            break;
        }
    }
    let text = String::from_utf8_lossy(&collected);
    let status = text
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    Ok((status, collected))
}
