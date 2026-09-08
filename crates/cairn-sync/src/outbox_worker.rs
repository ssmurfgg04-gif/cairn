//! Parallel outbox worker for concurrent chunk uploads.
//!
//! Uses a semaphore to limit concurrent uploads while processing multiple
//! outbox entries in parallel. Each worker picks up one outbox entry,
//! uploads the chunk, and acks or retries on failure.

use cairn_core::CairnError;
use cairn_proto::pb::JournalOp;
use cairn_store::OutboxEntry;
use prost::Message;
use std::sync::Arc;

/// Maximum concurrent uploads (default: 8)
pub const DEFAULT_MAX_CONCURRENT: usize = 8;

/// Callback for looking up lease tokens by file path.
pub type LeaseResolver = Arc<dyn Fn(&str) -> u64 + Send + Sync>;

fn extract_path(op_bytes: &[u8]) -> Option<String> {
    let op = JournalOp::decode(op_bytes).ok()?;
    match op.op.as_ref()? {
        cairn_proto::pb::journal_op::Op::FileUpsert(u) => Some(u.path.clone()),
        cairn_proto::pb::journal_op::Op::FileDelete(d) => Some(d.path.clone()),
        cairn_proto::pb::journal_op::Op::Rename(r) => Some(r.old_path.clone()),
        cairn_proto::pb::journal_op::Op::LeaseEvent(l) => Some(l.path.clone()),
    }
}

/// Trait for chunk upload - allows mocking in tests
#[async_trait::async_trait]
pub trait ChunkUploader: Send + Sync {
    /// Upload a chunk with explicit identity context (no store lookup here).
    async fn upload_chunk(
        &self,
        entry: &OutboxEntry,
        tenant: &str,
        device: &str,
        lease_token: u64,
    ) -> Result<(), CairnError>;
}

/// Default chunk uploader using the gRPC plane
pub struct GrpcChunkUploader {
    plane: Arc<dyn crate::plane::Plane>,
}

impl GrpcChunkUploader {
    pub fn new(plane: Arc<dyn crate::plane::Plane>) -> Self {
        Self { plane }
    }
}

#[async_trait::async_trait]
impl ChunkUploader for GrpcChunkUploader {
    async fn upload_chunk(
        &self,
        entry: &OutboxEntry,
        tenant: &str,
        device: &str,
        lease_token: u64,
    ) -> Result<(), CairnError> {
        let op: JournalOp = Message::decode(entry.op.as_slice()).map_err(|e| {
            CairnError::new(cairn_core::ErrorKind::Internal, format!("decode op: {e}"))
        })?;
        self.plane
            .append(
                tenant,
                &entry.project_id,
                device,
                &entry.request_id,
                op,
                lease_token,
            )
            .await
            .map_err(|e| {
                CairnError::new(
                    cairn_core::ErrorKind::Unavailable,
                    format!("plane append: {e}"),
                )
            })?;
        Ok(())
    }
}

/// Parallel outbox worker pool
pub struct OutboxWorker {
    outbox: Arc<dyn cairn_store::OutboxTrait>,
    uploader: Arc<dyn ChunkUploader>,
    semaphore: Arc<tokio::sync::Semaphore>,
    max_concurrent: usize,
    lease_resolver: Option<LeaseResolver>,
}

impl OutboxWorker {
    pub fn new(
        outbox: Arc<dyn cairn_store::OutboxTrait>,
        uploader: Arc<dyn ChunkUploader>,
        max_concurrent: usize,
    ) -> Self {
        let max_concurrent = max_concurrent.max(1);
        Self {
            outbox,
            uploader,
            semaphore: Arc::new(tokio::sync::Semaphore::new(max_concurrent)),
            max_concurrent,
            lease_resolver: None,
        }
    }

    #[must_use]
    pub fn with_lease_resolver(mut self, resolver: LeaseResolver) -> Self {
        self.lease_resolver = Some(resolver);
        self
    }

    pub fn with_defaults(
        outbox: Arc<dyn cairn_store::OutboxTrait>,
        uploader: Arc<dyn ChunkUploader>,
    ) -> Self {
        let max_concurrent = num_cpus::get() * 2;
        Self::new(outbox, uploader, max_concurrent)
    }

    /// Drain all pending entries for a project with bounded concurrency.
    /// Identity (tenant/device) is passed in by the caller (daemon), which
    /// owns the store — this crate never reaches into cairn-cli.
    /// Preserves per-path FIFO ordering while executing across paths in parallel.
    pub async fn run(
        &self,
        project_id: &str,
        tenant: &str,
        device: &str,
    ) -> Result<u64, CairnError> {
        let mut join_set = tokio::task::JoinSet::new();
        let mut processed: u64 = 0;
        let mut attempted = std::collections::HashSet::new();

        // 1. Group pending entries by path to guarantee FIFO per path.
        let entries = self.outbox.pending(project_id, 256);
        let mut groups: std::collections::BTreeMap<String, Vec<OutboxEntry>> =
            std::collections::BTreeMap::new();
        for entry in entries {
            if attempted.insert(entry.request_id.clone()) {
                let key = extract_path(&entry.op).unwrap_or_else(|| entry.request_id.clone());
                groups.entry(key).or_default().push(entry);
            }
        }

        // 2. Dispatch one task per path group under semaphore.
        for (_path, path_entries) in groups {
            let permit = self.semaphore.clone().acquire_owned().await.map_err(|e| {
                CairnError::new(cairn_core::ErrorKind::Internal, format!("semaphore: {e}"))
            })?;

            let outbox = self.outbox.clone();
            let uploader = self.uploader.clone();
            let tenant = tenant.to_owned();
            let device = device.to_owned();
            let resolver = self.lease_resolver.clone();

            join_set.spawn(async move {
                let _permit = permit;
                let mut path_processed = 0u64;
                for entry in path_entries {
                    let request_id = entry.request_id.clone();
                    let lease_token = match (&resolver, extract_path(&entry.op)) {
                        (Some(res), Some(p)) => res(&p),
                        _ => 0,
                    };

                    if let Err(e) = outbox.mark_attempt(&request_id, "sent") {
                        tracing::warn!(request_id = %request_id, "mark attempt failed: {e}");
                    }
                    match uploader.upload_chunk(&entry, &tenant, &device, lease_token).await {
                        Ok(()) => {
                            if let Err(e) = outbox.ack(&request_id) {
                                tracing::error!(request_id = %request_id, "ack failed: {e}");
                            }
                            path_processed += 1;
                        }
                        Err(e) => {
                            tracing::warn!(request_id = %request_id, "upload failed: {e}");
                            if let Err(e) = outbox.mark_attempt(&request_id, "pending") {
                                tracing::error!(request_id = %request_id, "mark retry failed: {e}");
                            }
                            // Stop processing later entries for this same path to maintain FIFO
                            break;
                        }
                    }
                }
                path_processed
            });
        }

        while let Some(result) = join_set.join_next().await {
            let n = result.map_err(|e| {
                CairnError::new(
                    cairn_core::ErrorKind::Internal,
                    format!("worker panicked: {e}"),
                )
            })?;
            processed += n;
        }

        tracing::info!(project_id = %project_id, processed, "outbox worker completed");
        Ok(processed)
    }

    pub fn max_concurrent(&self) -> usize {
        self.max_concurrent
    }

    pub fn available_permits(&self) -> usize {
        self.semaphore.available_permits()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cairn_core::ErrorKind;
    use cairn_store::{Outbox, OutboxEntry};
    use std::sync::{Arc, Mutex};
    use tempfile::tempdir;

    struct MockUploader {
        upload_count: Arc<tokio::sync::Mutex<usize>>,
        should_fail: bool,
    }

    #[async_trait::async_trait]
    impl ChunkUploader for MockUploader {
        async fn upload_chunk(
            &self,
            _entry: &OutboxEntry,
            _tenant: &str,
            _device: &str,
            _lease_token: u64,
        ) -> Result<(), CairnError> {
            let mut count = self.upload_count.lock().await;
            *count += 1;
            if self.should_fail {
                return Err(CairnError::new(ErrorKind::Unavailable, "mock failure"));
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            Ok(())
        }
    }

    fn create_test_outbox() -> (tempfile::TempDir, Arc<dyn cairn_store::OutboxTrait>) {
        let dir = tempdir().unwrap();
        let conn = rusqlite::Connection::open(dir.path().join("test.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE outbox(request_id TEXT PRIMARY KEY, project_id TEXT, op BLOB,
             state TEXT, attempts INTEGER, created_at INTEGER);",
        )
        .unwrap();
        let outbox = Outbox::new(Arc::new(Mutex::new(conn)));
        (dir, Arc::new(outbox))
    }

    #[tokio::test]
    async fn parallel_worker_processes_multiple_entries() {
        let (dir, outbox) = create_test_outbox();
        let upload_count = Arc::new(tokio::sync::Mutex::new(0));
        for i in 0..10 {
            outbox
                .enqueue(OutboxEntry {
                    request_id: format!("req-{i}"),
                    project_id: "test-project".into(),
                    op: vec![1, 2, 3],
                    state: "pending".into(),
                    attempts: 0,
                    created_at: i64::from(i),
                })
                .unwrap();
        }
        let uploader = Arc::new(MockUploader {
            upload_count: upload_count.clone(),
            should_fail: false,
        });
        let worker = OutboxWorker::new(outbox.clone(), uploader, 4);
        let start = std::time::Instant::now();
        let n = worker.run("test-project", "t1", "dev-1").await.unwrap();
        let elapsed = start.elapsed();
        assert_eq!(n, 10);
        assert_eq!(*upload_count.lock().await, 10);
        // 10 x 10ms serialized = 100ms floor; 4 workers should beat it well.
        // 500ms gives headroom for debug builds on loaded boxes.
        assert!(elapsed < std::time::Duration::from_millis(500));
        assert_eq!(outbox.pending_count("test-project"), 0);
        drop(dir);
    }

    #[tokio::test]
    async fn parallel_worker_handles_failures() {
        let (dir, outbox) = create_test_outbox();
        let upload_count = Arc::new(tokio::sync::Mutex::new(0));
        for i in 0..5 {
            outbox
                .enqueue(OutboxEntry {
                    request_id: format!("req-{i}"),
                    project_id: "test-project".into(),
                    op: vec![1, 2, 3],
                    state: "pending".into(),
                    attempts: 0,
                    created_at: i64::from(i),
                })
                .unwrap();
        }
        let uploader = Arc::new(MockUploader {
            upload_count: upload_count.clone(),
            should_fail: true,
        });
        let worker = OutboxWorker::new(outbox.clone(), uploader, 2);
        worker.run("test-project", "t1", "dev-1").await.unwrap();
        assert_eq!(*upload_count.lock().await, 5);
        assert_eq!(outbox.pending_count("test-project"), 5);
        drop(dir);
    }

    #[tokio::test]
    async fn semaphore_limits_concurrency() {
        let (dir, outbox) = create_test_outbox();
        for i in 0..10 {
            outbox
                .enqueue(OutboxEntry {
                    request_id: format!("req-{i}"),
                    project_id: "test-project".into(),
                    op: vec![1, 2, 3],
                    state: "pending".into(),
                    attempts: 0,
                    created_at: i64::from(i),
                })
                .unwrap();
        }
        let uploader = Arc::new(MockUploader {
            upload_count: Arc::new(tokio::sync::Mutex::new(0)),
            should_fail: false,
        });
        let worker = OutboxWorker::new(outbox, uploader, 2);
        assert_eq!(worker.available_permits(), 2);
        assert_eq!(worker.max_concurrent(), 2);
        drop(dir);
    }
}
