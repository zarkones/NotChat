//! Local SQLite: profile, contacts, requests, messages, drafts, outbox, seen nonces.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

use crate::crypto::{signing_key_from_bytes, now_unix_ms};
use crate::identity::{ensure_private_dir, Identity};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContactStatus {
    Pending,
    Accepted,
    Rejected,
    Blocked,
}

impl ContactStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Accepted => "accepted",
            Self::Rejected => "rejected",
            Self::Blocked => "blocked",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "pending" => Ok(Self::Pending),
            "accepted" => Ok(Self::Accepted),
            "rejected" => Ok(Self::Rejected),
            "blocked" => Ok(Self::Blocked),
            other => Err(anyhow!("unknown contact status {other}")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Contact {
    pub id: String,
    pub pk: [u8; 32],
    pub self_nick: String,
    pub custom_nick: Option<String>,
    pub status: ContactStatus,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Contact {
    /// Display label: custom nick → latest self-nick → short id prefix.
    pub fn display_name(&self) -> String {
        if let Some(c) = &self.custom_nick {
            if !c.is_empty() {
                return c.clone();
            }
        }
        if !self.self_nick.is_empty() {
            return self.self_nick.clone();
        }
        format!("{}…", &self.id[..8.min(self.id.len())])
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContactRequest {
    pub rowid: i64,
    pub from_id: String,
    pub from_pk: [u8; 32],
    pub from_nick: String,
    pub to_nonce: [u8; 16],
    pub ts: i64,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub msg_id: String,
    pub peer_id: String,
    pub direction: String, // in | out
    pub plaintext: String,
    pub ts: i64,
    pub status: String,
}

#[derive(Debug, Clone)]
pub struct OutboxItem {
    pub id: i64,
    pub peer_id: String,
    pub kind: String,
    pub payload: String,
    pub created_at: i64,
    pub last_attempt: Option<i64>,
    pub attempts: i64,
}

pub struct Db {
    conn: Connection,
    path: PathBuf,
}

impl Db {
    pub fn open(data_root: &Path) -> Result<Self> {
        ensure_private_dir(data_root)?;
        let path = data_root.join("onion-chat.sqlite3");
        let conn = Connection::open(&path)
            .with_context(|| format!("open sqlite {}", path.display()))?;
        conn.execute_batch(
            "
            PRAGMA journal_mode=WAL;
            PRAGMA foreign_keys=ON;
            ",
        )?;
        let mut db = Self { conn, path };
        db.migrate()?;
        Ok(db)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn migrate(&mut self) -> Result<()> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS self_profile (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                nick TEXT NOT NULL DEFAULT '',
                signing_sk BLOB NOT NULL,
                signing_pk BLOB NOT NULL,
                onion_id TEXT,
                invite_nonce BLOB NOT NULL
            );

            CREATE TABLE IF NOT EXISTS contacts (
                id TEXT PRIMARY KEY,
                pk BLOB NOT NULL,
                self_nick TEXT NOT NULL DEFAULT '',
                custom_nick TEXT,
                status TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS contact_requests (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                from_id TEXT NOT NULL,
                from_pk BLOB NOT NULL,
                from_nick TEXT NOT NULL,
                to_nonce BLOB NOT NULL,
                ts INTEGER NOT NULL,
                status TEXT NOT NULL DEFAULT 'pending',
                UNIQUE(from_id, to_nonce)
            );

            CREATE TABLE IF NOT EXISTS messages (
                msg_id TEXT PRIMARY KEY,
                peer_id TEXT NOT NULL,
                direction TEXT NOT NULL,
                plaintext TEXT NOT NULL,
                ts INTEGER NOT NULL,
                status TEXT NOT NULL DEFAULT 'sent'
            );

            CREATE INDEX IF NOT EXISTS idx_messages_peer_ts
                ON messages(peer_id, ts);

            CREATE TABLE IF NOT EXISTS outbox (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                peer_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                payload TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                last_attempt INTEGER,
                attempts INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS seen_nonces (
                nonce_key TEXT PRIMARY KEY,
                seen_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS drafts (
                peer_id TEXT PRIMARY KEY,
                text TEXT NOT NULL,
                updated_at INTEGER NOT NULL
            );
            "#,
        )?;
        Ok(())
    }

    /// Load identity or generate + persist a new one.
    pub fn load_or_create_identity(&mut self) -> Result<Identity> {
        let row: Option<(String, Vec<u8>, Vec<u8>, Option<String>, Vec<u8>)> = self
            .conn
            .query_row(
                "SELECT nick, signing_sk, signing_pk, onion_id, invite_nonce FROM self_profile WHERE id=1",
                [],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                    ))
                },
            )
            .optional()?;

        if let Some((nick, sk, pk, onion_id, nonce)) = row {
            let signing = signing_key_from_bytes(&sk)?;
            if signing.verifying_key().to_bytes().as_slice() != pk.as_slice() {
                return Err(anyhow!("self_profile pk mismatch"));
            }
            if nonce.len() != 16 {
                return Err(anyhow!("bad invite_nonce length"));
            }
            let mut invite_nonce = [0u8; 16];
            invite_nonce.copy_from_slice(&nonce);
            return Ok(Identity {
                signing,
                invite_nonce,
                nick,
                onion_id,
            });
        }

        let identity = Identity::generate("");
        self.conn.execute(
            "INSERT INTO self_profile (id, nick, signing_sk, signing_pk, onion_id, invite_nonce)
             VALUES (1, ?1, ?2, ?3, NULL, ?4)",
            params![
                identity.nick,
                identity.secret_key_bytes().as_slice(),
                identity.public_key_bytes().as_slice(),
                identity.invite_nonce.as_slice(),
            ],
        )?;
        Ok(identity)
    }

    pub fn save_identity(&mut self, identity: &Identity) -> Result<()> {
        self.conn.execute(
            "UPDATE self_profile SET nick=?1, signing_sk=?2, signing_pk=?3, onion_id=?4, invite_nonce=?5 WHERE id=1",
            params![
                identity.nick,
                identity.secret_key_bytes().as_slice(),
                identity.public_key_bytes().as_slice(),
                identity.onion_id,
                identity.invite_nonce.as_slice(),
            ],
        )?;
        Ok(())
    }

    pub fn set_nick(&mut self, nick: &str) -> Result<()> {
        self.conn
            .execute("UPDATE self_profile SET nick=?1 WHERE id=1", params![nick])?;
        Ok(())
    }

    pub fn set_onion_id(&mut self, onion_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE self_profile SET onion_id=?1 WHERE id=1",
            params![onion_id],
        )?;
        Ok(())
    }

    pub fn rotate_invite_nonce(&mut self, nonce: &[u8; 16]) -> Result<()> {
        self.conn.execute(
            "UPDATE self_profile SET invite_nonce=?1 WHERE id=1",
            params![nonce.as_slice()],
        )?;
        Ok(())
    }

    // ----- seen nonces / msg ids -----

    pub fn has_seen(&self, key: &str) -> Result<bool> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(1) FROM seen_nonces WHERE nonce_key=?1",
            params![key],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    pub fn mark_seen(&mut self, key: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO seen_nonces (nonce_key, seen_at) VALUES (?1, ?2)",
            params![key, now_unix_ms()],
        )?;
        Ok(())
    }

    // ----- contacts -----

    pub fn upsert_contact(&mut self, c: &Contact) -> Result<()> {
        self.conn.execute(
            "INSERT INTO contacts (id, pk, self_nick, custom_nick, status, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(id) DO UPDATE SET
               pk=excluded.pk,
               self_nick=excluded.self_nick,
               custom_nick=COALESCE(excluded.custom_nick, contacts.custom_nick),
               status=excluded.status,
               updated_at=excluded.updated_at",
            params![
                c.id,
                c.pk.as_slice(),
                c.self_nick,
                c.custom_nick,
                c.status.as_str(),
                c.created_at,
                c.updated_at,
            ],
        )?;
        Ok(())
    }

    pub fn get_contact(&self, id: &str) -> Result<Option<Contact>> {
        self.conn
            .query_row(
                "SELECT id, pk, self_nick, custom_nick, status, created_at, updated_at
                 FROM contacts WHERE id=?1",
                params![id],
                |r| {
                    let pk: Vec<u8> = r.get(1)?;
                    Ok((
                        r.get::<_, String>(0)?,
                        pk,
                        r.get::<_, String>(2)?,
                        r.get::<_, Option<String>>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, i64>(5)?,
                        r.get::<_, i64>(6)?,
                    ))
                },
            )
            .optional()?
            .map(|(id, pk, self_nick, custom_nick, status, created_at, updated_at)| {
                if pk.len() != 32 {
                    return Err(anyhow!("bad contact pk length"));
                }
                let mut arr = [0u8; 32];
                arr.copy_from_slice(&pk);
                Ok(Contact {
                    id,
                    pk: arr,
                    self_nick,
                    custom_nick,
                    status: ContactStatus::parse(&status)?,
                    created_at,
                    updated_at,
                })
            })
            .transpose()
    }

    pub fn list_contacts(&self) -> Result<Vec<Contact>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, pk, self_nick, custom_nick, status, created_at, updated_at
             FROM contacts WHERE status='accepted' ORDER BY updated_at DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Vec<u8>>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, i64>(6)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, pk, self_nick, custom_nick, status, created_at, updated_at) = row?;
            if pk.len() != 32 {
                continue;
            }
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&pk);
            out.push(Contact {
                id,
                pk: arr,
                self_nick,
                custom_nick,
                status: ContactStatus::parse(&status)?,
                created_at,
                updated_at,
            });
        }
        Ok(out)
    }

    pub fn set_contact_status(&mut self, id: &str, status: ContactStatus) -> Result<()> {
        self.conn.execute(
            "UPDATE contacts SET status=?1, updated_at=?2 WHERE id=?3",
            params![status.as_str(), now_unix_ms(), id],
        )?;
        Ok(())
    }

    pub fn set_custom_nick(&mut self, id: &str, nick: Option<&str>) -> Result<()> {
        self.conn.execute(
            "UPDATE contacts SET custom_nick=?1, updated_at=?2 WHERE id=?3",
            params![nick, now_unix_ms(), id],
        )?;
        Ok(())
    }

    pub fn update_contact_self_nick(&mut self, id: &str, nick: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE contacts SET self_nick=?1, updated_at=?2 WHERE id=?3",
            params![nick, now_unix_ms(), id],
        )?;
        Ok(())
    }

    // ----- contact requests -----

    pub fn insert_contact_request(
        &mut self,
        from_id: &str,
        from_pk: &[u8; 32],
        from_nick: &str,
        to_nonce: &[u8; 16],
        ts: i64,
    ) -> Result<bool> {
        let changed = self.conn.execute(
            "INSERT OR IGNORE INTO contact_requests
             (from_id, from_pk, from_nick, to_nonce, ts, status)
             VALUES (?1, ?2, ?3, ?4, ?5, 'pending')",
            params![
                from_id,
                from_pk.as_slice(),
                from_nick,
                to_nonce.as_slice(),
                ts
            ],
        )?;
        Ok(changed > 0)
    }

    pub fn list_pending_requests(&self) -> Result<Vec<ContactRequest>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, from_id, from_pk, from_nick, to_nonce, ts, status
             FROM contact_requests WHERE status='pending' ORDER BY ts DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Vec<u8>>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Vec<u8>>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, String>(6)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (rowid, from_id, from_pk, from_nick, to_nonce, ts, status) = row?;
            if from_pk.len() != 32 || to_nonce.len() != 16 {
                continue;
            }
            let mut pk = [0u8; 32];
            pk.copy_from_slice(&from_pk);
            let mut nonce = [0u8; 16];
            nonce.copy_from_slice(&to_nonce);
            out.push(ContactRequest {
                rowid,
                from_id,
                from_pk: pk,
                from_nick,
                to_nonce: nonce,
                ts,
                status,
            });
        }
        Ok(out)
    }

    pub fn set_request_status(&mut self, rowid: i64, status: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE contact_requests SET status=?1 WHERE id=?2",
            params![status, rowid],
        )?;
        Ok(())
    }

    pub fn get_request(&self, rowid: i64) -> Result<Option<ContactRequest>> {
        self.conn
            .query_row(
                "SELECT id, from_id, from_pk, from_nick, to_nonce, ts, status
                 FROM contact_requests WHERE id=?1",
                params![rowid],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Vec<u8>>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, Vec<u8>>(4)?,
                        r.get::<_, i64>(5)?,
                        r.get::<_, String>(6)?,
                    ))
                },
            )
            .optional()?
            .map(|(rowid, from_id, from_pk, from_nick, to_nonce, ts, status)| {
                if from_pk.len() != 32 || to_nonce.len() != 16 {
                    return Err(anyhow!("bad request blob lengths"));
                }
                let mut pk = [0u8; 32];
                pk.copy_from_slice(&from_pk);
                let mut nonce = [0u8; 16];
                nonce.copy_from_slice(&to_nonce);
                Ok(ContactRequest {
                    rowid,
                    from_id,
                    from_pk: pk,
                    from_nick,
                    to_nonce: nonce,
                    ts,
                    status,
                })
            })
            .transpose()
    }

    // ----- messages -----

    pub fn insert_message(&mut self, m: &Message) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO messages (msg_id, peer_id, direction, plaintext, ts, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![m.msg_id, m.peer_id, m.direction, m.plaintext, m.ts, m.status],
        )?;
        Ok(())
    }

    pub fn list_messages(&self, peer_id: &str, limit: i64) -> Result<Vec<Message>> {
        let mut stmt = self.conn.prepare(
            "SELECT msg_id, peer_id, direction, plaintext, ts, status
             FROM messages WHERE peer_id=?1 ORDER BY ts ASC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![peer_id, limit], |r| {
            Ok(Message {
                msg_id: r.get(0)?,
                peer_id: r.get(1)?,
                direction: r.get(2)?,
                plaintext: r.get(3)?,
                ts: r.get(4)?,
                status: r.get(5)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Newest message for a peer, if any.
    pub fn last_message(&self, peer_id: &str) -> Result<Option<Message>> {
        self.conn
            .query_row(
                "SELECT msg_id, peer_id, direction, plaintext, ts, status
                 FROM messages WHERE peer_id=?1 ORDER BY ts DESC LIMIT 1",
                params![peer_id],
                |r| {
                    Ok(Message {
                        msg_id: r.get(0)?,
                        peer_id: r.get(1)?,
                        direction: r.get(2)?,
                        plaintext: r.get(3)?,
                        ts: r.get(4)?,
                        status: r.get(5)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    /// Accepted contacts with optional last message, newest activity first.
    pub fn list_conversations(&self) -> Result<Vec<(Contact, Option<Message>)>> {
        let contacts = self.list_contacts()?;
        let mut out = Vec::with_capacity(contacts.len());
        for c in contacts {
            let last = self.last_message(&c.id)?;
            out.push((c, last));
        }
        out.sort_by(|a, b| {
            let ta = a.1.as_ref().map(|m| m.ts).unwrap_or(a.0.updated_at);
            let tb = b.1.as_ref().map(|m| m.ts).unwrap_or(b.0.updated_at);
            tb.cmp(&ta)
        });
        Ok(out)
    }

    pub fn update_message_status(&mut self, msg_id: &str, status: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE messages SET status=?1 WHERE msg_id=?2",
            params![status, msg_id],
        )?;
        Ok(())
    }

    // ----- drafts (composer text per peer) -----

    pub fn get_draft(&self, peer_id: &str) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT text FROM drafts WHERE peer_id=?1",
                params![peer_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    /// Persist composer text for a peer. Empty text clears the draft.
    pub fn set_draft(&mut self, peer_id: &str, text: &str) -> Result<()> {
        let trimmed = text; // callers may pass raw composer value (incl. whitespace-only)
        if trimmed.is_empty() {
            return self.clear_draft(peer_id);
        }
        self.conn.execute(
            "INSERT INTO drafts (peer_id, text, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(peer_id) DO UPDATE SET text=excluded.text, updated_at=excluded.updated_at",
            params![peer_id, trimmed, now_unix_ms()],
        )?;
        Ok(())
    }

    pub fn clear_draft(&mut self, peer_id: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM drafts WHERE peer_id=?1", params![peer_id])?;
        Ok(())
    }

    // ----- outbox -----


    pub fn enqueue_outbox(&mut self, peer_id: &str, kind: &str, payload: &str) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO outbox (peer_id, kind, payload, created_at, attempts)
             VALUES (?1, ?2, ?3, ?4, 0)",
            params![peer_id, kind, payload, now_unix_ms()],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn list_outbox(&self, limit: i64) -> Result<Vec<OutboxItem>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, peer_id, kind, payload, created_at, last_attempt, attempts
             FROM outbox ORDER BY id ASC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |r| {
            Ok(OutboxItem {
                id: r.get(0)?,
                peer_id: r.get(1)?,
                kind: r.get(2)?,
                payload: r.get(3)?,
                created_at: r.get(4)?,
                last_attempt: r.get(5)?,
                attempts: r.get(6)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn mark_outbox_attempt(&mut self, id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE outbox SET attempts=attempts+1, last_attempt=?1 WHERE id=?2",
            params![now_unix_ms(), id],
        )?;
        Ok(())
    }

    pub fn remove_outbox(&mut self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM outbox WHERE id=?1", params![id])?;
        Ok(())
    }
}
