//! Durable outbox for pending journal appends (SPEC §5.3/§7.3).
//!
//! The outbox is the I2 core on the client: an op is durable HERE before any upload or append
//! is considered acknowledged; recovery = outbox resend (§7.3). Entries record attempts; the
//! sync engine retries with full jitter (ADR-0010) and removes entries only after the server
//! acknowledges the append (request_id dedup makes resend safe).

use cairn_core::CairnError;
use rusqlite::Connection;
use std::sync::{Arc, Mutex};

/// One pending journal append.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxEntry {
    /// UUIDv7 request id (server idempotency key).
    pub request_id: String,
    /// Project id.
    pub project_id: String,
    /// Serialized JournalOp (prost bytes).
    pub op: Vec<u8>,
    /// 'pending' | 'sent' (sent until server ack)
    pub state: String,
    /// Send attempts so far.
    pub attempts: u32,
    /// Creation timestamp (client, informational per I4).
    pub created_at: i64,
}

/// Outbox API over the client store's connection.
#[derive(Clone)]
pub struct Outbox {
    db: Arc<Mutex<Connection>>,
}

impl Outbox {
    /// New outbox over the shared connection.
    #[must_use]
    pub fn new(db: Arc<Mutex<Connection>>) -> Self {
        Outbox { db }
    }

    /// Enqueue an append (durability point: committed before returning).
    pub fn enqueue(&self, entry: OutboxEntry) -> Result<(), CairnError> {
        let db = self.db.lock().expect("outbox poisoned");
        db.execute(
            "INSERT INTO outbox(request_id, project_id, op, state, attempts, created_at)
             VALUES(?1,?2,?3,?4,?5,?6)
             ON CONFLICT(request_id) DO NOTHING",
            rusqlite::params![
                entry.request_id,
                entry.project_id,
                entry.op,
                entry.state,
                entry.attempts as i64,
                entry.created_at
            ],
        )
        .map_err(|e| CairnError::new(cairn_core::ErrorKind::Io, format!("outbox enqueue: {e}")))?;
        Ok(())
    }

    /// Pending entries in FIFO order (recovery = resend these).
    #[must_use]
    pub fn pending(&self, project_id: &str, limit: usize) -> Vec<OutboxEntry> {
        let db = self.db.lock().expect("outbox poisoned");
        let mut stmt = match db.prepare(
            "SELECT request_id, project_id, op, state, attempts, created_at
             FROM outbox WHERE project_id=?1 AND state IN ('pending','sent')
             ORDER BY created_at ASC LIMIT ?2",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt
            .query_map(rusqlite::params![project_id, limit as i64], row_to_entry)
            .ok();
        rows.map(|rows| rows.filter_map(|r| r.ok()).collect()).unwrap_or_default()
    }

    /// Bump attempts after a failed send.
    pub fn mark_attempt(&self, request_id: &str, new_state: &str) -> Result<(), CairnError> {
        let db = self.db.lock().expect("outbox poisoned");
        db.execute(
            "UPDATE outbox SET attempts=attempts+1, state=?2 WHERE request_id=?1",
            rusqlite::params![request_id, new_state],
        )
        .map_err(|e| CairnError::new(cairn_core::ErrorKind::Io, format!("outbox attempt: {e}")))?;
        Ok(())
    }

    /// Remove after server ack (dedup makes re-send idempotent; removal is safe).
    pub fn ack(&self, request_id: &str) -> Result<(), CairnError> {
        let db = self.db.lock().expect("outbox poisoned");
        db.execute("DELETE FROM outbox WHERE request_id=?1", rusqlite::params![request_id])
            .map_err(|e| CairnError::new(cairn_core::ErrorKind::Io, format!("outbox ack: {e}")))?;
        Ok(())
    }

    /// Count of pending entries (status/doctor surface).
    #[must_use]
    pub fn pending_count(&self, project_id: &str) -> u64 {
        let db = self.db.lock().expect("outbox poisoned");
        db.query_row(
            "SELECT COUNT(*) FROM outbox WHERE project_id=?1 AND state IN ('pending','sent')",
            rusqlite::params![project_id],
            |r| r.get::<_, i64>(0),
        )
        .map(|v| v.max(0) as u64)
        .unwrap_or(0)
    }
}

fn row_to_entry(r: &rusqlite::Row<'_>) -> rusqlite::Result<OutboxEntry> {
    Ok(OutboxEntry {
        request_id: r.get(0)?,
        project_id: r.get(1)?,
        op: r.get(2)?,
        state: r.get(3)?,
        attempts: r.get::<_, i64>(4)?.max(0) as u32,
        created_at: r.get(5)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_outbox() -> (tempfile::TempDir, Outbox) {
        let dir = tempfile::tempdir().unwrap();
        let conn = Connection::open(dir.path().join("o.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE outbox(request_id TEXT PRIMARY KEY, project_id TEXT, op BLOB,
             state TEXT, attempts INTEGER, created_at INTEGER);",
        )
        .unwrap();
        (dir, Outbox::new(Arc::new(Mutex::new(conn))))
    }

    #[test]
    fn enqueue_pending_ack_cycle() {
        let (_d, ob) = open_outbox();
        let e = OutboxEntry {
            request_id: "req-1".into(),
            project_id: "p1".into(),
            op: vec![1, 2, 3],
            state: "pending".into(),
            attempts: 0,
            created_at: 1,
        };
        ob.enqueue(e).unwrap();
        assert_eq!(ob.pending_count("p1"), 1);
        let pend = ob.pending("p1", 10);
        assert_eq!(pend.len(), 1);
        assert_eq!(pend[0].request_id, "req-1");
        ob.mark_attempt("req-1", "sent").unwrap();
        assert_eq!(ob.pending("p1", 10)[0].attempts, 1);
        ob.ack("req-1").unwrap();
        assert_eq!(ob.pending_count("p1"), 0);
    }

    #[test]
    fn duplicate_request_id_is_idempotent() {
        let (_d, ob) = open_outbox();
        let mk = || OutboxEntry {
            request_id: "same".into(),
            project_id: "p1".into(),
            op: vec![9],
            state: "pending".into(),
            attempts: 0,
            created_at: 1,
        };
        ob.enqueue(mk()).unwrap();
        ob.enqueue(mk()).unwrap();
        assert_eq!(ob.pending_count("p1"), 1, "ON CONFLICT DO NOTHING dedupes");
    }
}
