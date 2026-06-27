//! Reads new messages directly from the macOS Messages SQLite database.

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};

use super::attributed_body;

/// One inbound iMessage row relevant to the gateway.
#[derive(Debug, Clone)]
pub struct Message {
    pub row_id: i64,
    /// Sender handle (phone/email); empty for self-sent messages.
    pub handle: String,
    /// The chat's identifier (the handle for 1:1 chats).
    pub chat_identifier: String,
    pub text: String,
    pub is_from_me: bool,
}

/// Poller reads `chat.db` read-only via rusqlite.
#[derive(Clone)]
pub struct Poller {
    db_path: String,
}

// Only real text messages: exclude tapbacks/reactions (associated_message_type)
// and system rows like group renames or joins (item_type).
const SELECT_NEW: &str = "\
SELECT m.ROWID, m.text, m.attributedBody, m.is_from_me, \
       COALESCE(h.id, ''), COALESCE(c.chat_identifier, '') \
FROM message m \
LEFT JOIN handle h ON m.handle_id = h.ROWID \
LEFT JOIN chat_message_join cmj ON cmj.message_id = m.ROWID \
LEFT JOIN chat c ON c.ROWID = cmj.chat_id \
WHERE m.ROWID > ?1 AND m.associated_message_type = 0 AND m.item_type = 0 \
ORDER BY m.ROWID ASC";

impl Poller {
    pub fn new(db_path: String) -> Self {
        Self { db_path }
    }

    fn open(&self) -> Result<Connection> {
        Connection::open_with_flags(&self.db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("open {}", self.db_path))
    }

    /// Returns messages with ROWID greater than `since`, oldest first. Decodes
    /// `attributedBody` when `text` is NULL. This is blocking; call it from a
    /// blocking context.
    pub fn poll(&self, since: i64) -> Result<Vec<Message>> {
        let conn = self.open()?;
        let mut stmt = conn.prepare(SELECT_NEW)?;
        let rows = stmt.query_map([since], |row| {
            let text: Option<String> = row.get(1)?;
            let body: Option<Vec<u8>> = row.get(2)?;
            let is_from_me: i64 = row.get(3)?;
            Ok(Message {
                row_id: row.get(0)?,
                text: text.unwrap_or_else(|| {
                    body.map(|b| attributed_body::decode(&b))
                        .unwrap_or_default()
                }),
                handle: row.get(4)?,
                chat_identifier: row.get(5)?,
                is_from_me: is_from_me == 1,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Highest message ROWID in the database, or 0 when empty. Used to skip
    /// backlog on first run.
    pub fn max_row_id(&self) -> Result<i64> {
        let conn = self.open()?;
        let v: i64 = conn.query_row("SELECT COALESCE(MAX(ROWID), 0) FROM message", [], |r| {
            r.get(0)
        })?;
        Ok(v)
    }
}
