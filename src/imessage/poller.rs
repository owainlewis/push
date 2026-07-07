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
    /// Group chats are intentionally out of scope for the iMessage channel.
    pub is_group: bool,
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
       COALESCE(h.id, ''), COALESCE(c.chat_identifier, ''), \
       COALESCE((SELECT COUNT(*) FROM chat_handle_join chj WHERE chj.chat_id = c.ROWID), 0) \
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
            let participant_count: i64 = row.get(6)?;
            Ok(Message {
                row_id: row.get(0)?,
                text: text.unwrap_or_else(|| {
                    body.map(|b| attributed_body::decode(&b))
                        .unwrap_or_default()
                }),
                handle: row.get(4)?,
                chat_identifier: row.get(5)?,
                is_group: participant_count > 1,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::temp_path;
    use rusqlite::Connection;

    #[test]
    fn poll_decodes_text_filters_system_rows_and_marks_groups() {
        let path = temp_path("chat-db");
        let conn = Connection::open(&path).unwrap();
        create_schema(&conn);
        insert_handle(&conn, 1, "+15551234567");
        insert_handle(&conn, 2, "+15557654321");
        insert_chat(&conn, 1, "+15551234567");
        insert_chat(&conn, 2, "chat123456789");
        insert_chat_handle(&conn, 1, 1);
        insert_chat_handle(&conn, 2, 1);
        insert_chat_handle(&conn, 2, 2);
        insert_message(&conn, 1, Some("hello"), None, 0, 0, 0, 1, 1);
        insert_message(&conn, 2, Some("tapback"), None, 0, 2000, 0, 1, 1);
        insert_message(&conn, 3, Some("system"), None, 0, 0, 1, 1, 1);
        insert_message(&conn, 4, Some("group"), None, 0, 0, 0, 1, 2);
        drop(conn);

        let got = Poller::new(path.to_string_lossy().to_string())
            .poll(0)
            .unwrap();

        assert_eq!(got.len(), 2);
        assert_eq!(got[0].row_id, 1);
        assert_eq!(got[0].text, "hello");
        assert!(!got[0].is_group);
        assert_eq!(got[1].row_id, 4);
        assert!(got[1].is_group);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn poll_uses_attributed_body_when_text_is_null() {
        let path = temp_path("chat-db-attributed");
        let conn = Connection::open(&path).unwrap();
        create_schema(&conn);
        insert_handle(&conn, 1, "owain@example.com");
        insert_chat(&conn, 1, "owain@example.com");
        insert_chat_handle(&conn, 1, 1);
        insert_message(
            &conn,
            1,
            None,
            Some(&attributed_blob("from body")),
            0,
            0,
            0,
            1,
            1,
        );
        drop(conn);

        let got = Poller::new(path.to_string_lossy().to_string())
            .poll(0)
            .unwrap();

        assert_eq!(got[0].text, "from body");

        let _ = std::fs::remove_file(path);
    }

    fn create_schema(conn: &Connection) {
        conn.execute_batch(
            "
            CREATE TABLE message (
                ROWID INTEGER PRIMARY KEY,
                text TEXT,
                attributedBody BLOB,
                is_from_me INTEGER NOT NULL,
                associated_message_type INTEGER NOT NULL,
                item_type INTEGER NOT NULL,
                handle_id INTEGER
            );
            CREATE TABLE handle (ROWID INTEGER PRIMARY KEY, id TEXT NOT NULL);
            CREATE TABLE chat (ROWID INTEGER PRIMARY KEY, chat_identifier TEXT NOT NULL);
            CREATE TABLE chat_message_join (message_id INTEGER NOT NULL, chat_id INTEGER NOT NULL);
            CREATE TABLE chat_handle_join (chat_id INTEGER NOT NULL, handle_id INTEGER NOT NULL);
            ",
        )
        .unwrap();
    }

    fn insert_handle(conn: &Connection, row_id: i64, id: &str) {
        conn.execute(
            "INSERT INTO handle (ROWID, id) VALUES (?1, ?2)",
            (row_id, id),
        )
        .unwrap();
    }

    fn insert_chat(conn: &Connection, row_id: i64, id: &str) {
        conn.execute(
            "INSERT INTO chat (ROWID, chat_identifier) VALUES (?1, ?2)",
            (row_id, id),
        )
        .unwrap();
    }

    fn insert_chat_handle(conn: &Connection, chat_id: i64, handle_id: i64) {
        conn.execute(
            "INSERT INTO chat_handle_join (chat_id, handle_id) VALUES (?1, ?2)",
            (chat_id, handle_id),
        )
        .unwrap();
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_message(
        conn: &Connection,
        row_id: i64,
        text: Option<&str>,
        body: Option<&[u8]>,
        is_from_me: i64,
        associated_message_type: i64,
        item_type: i64,
        handle_id: i64,
        chat_id: i64,
    ) {
        conn.execute(
            "INSERT INTO message (ROWID, text, attributedBody, is_from_me, associated_message_type, item_type, handle_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            (row_id, text, body, is_from_me, associated_message_type, item_type, handle_id),
        )
        .unwrap();
        conn.execute(
            "INSERT INTO chat_message_join (message_id, chat_id) VALUES (?1, ?2)",
            (row_id, chat_id),
        )
        .unwrap();
    }

    fn attributed_blob(text: &str) -> Vec<u8> {
        let mut b = b"\x04\x0bstreamtyped\x81NSString\x01\x94\x84\x01".to_vec();
        b.push(0x2b);
        b.push(text.len() as u8);
        b.extend_from_slice(text.as_bytes());
        b
    }
}
