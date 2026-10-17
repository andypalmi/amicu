//! Uploading queued reports: batch packing, response handling and backoff.
//!
//! The HTTP client is behind [`Transport`] so this logic is tested without a
//! network. A batch is the oldest queued reports that fit in
//! [`MAX_BATCH_BYTES`]. The endpoint answers `202 Accepted`; any 2xx counts as
//! delivered and the batch leaves the queue. `400`, `413` and `422` mean the
//! server will never take that batch, so it is dropped rather than retried
//! forever. Everything else (a `503`, a timeout, DNS failure, a `404` while the
//! endpoint is not live yet) keeps the batch queued and backs off.

use std::future::Future;
use std::time::Duration;

use crate::identity::BatchId;
use crate::queue::{DiskQueue, QueueEntry, QueueError};
use crate::report::{MAX_BATCH_BYTES, MAX_BATCH_REPORTS, TELEMETRY_SCHEMA_VERSION, TelemetryBatch};

/// Posts one JSON batch body; returns the HTTP status, or a coarse reason
/// when no response arrived.
pub trait Transport {
    /// POST `body` with `idempotency_key` (the batch id) as a header.
    fn post(
        &self,
        body: String,
        idempotency_key: String,
    ) -> impl Future<Output = Result<u16, String>>;
}

/// What a response means for the batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// Stored (2xx): remove from the queue.
    Accepted,
    /// Refused for good (400/413/422): drop from the queue.
    Rejected,
    /// Not now (5xx, 429, 404, network): keep and back off.
    Retry,
}

impl Delivery {
    /// Classify a status code.
    #[must_use]
    pub fn of_status(status: u16) -> Self {
        match status {
            200..=299 => Self::Accepted,
            400 | 413 | 422 => Self::Rejected,
            _ => Self::Retry,
        }
    }
}

/// Exponential backoff with jitter: the delay doubles per consecutive failure
/// up to `max`, and each delay is drawn from its upper half so agents that
/// failed together do not retry together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    base: Duration,
    max: Duration,
    failures: u32,
}

impl Backoff {
    /// Backoff from `base`, capped at `max`.
    #[must_use]
    pub fn new(base: Duration, max: Duration) -> Self {
        Self {
            base,
            max,
            failures: 0,
        }
    }

    /// Record a failure and return how long to wait. `jitter` is a uniform
    /// random number in `[0, 1)`.
    pub fn fail(&mut self, jitter: f64) -> Duration {
        let exp = self
            .base
            .saturating_mul(2u32.saturating_pow(self.failures.min(30)));
        self.failures = self.failures.saturating_add(1);
        let ceiling = exp.min(self.max);
        ceiling.mul_f64(0.5 + 0.5 * jitter.clamp(0.0, 1.0))
    }

    /// Record a success.
    pub fn reset(&mut self) {
        self.failures = 0;
    }

    /// Consecutive failures so far.
    #[must_use]
    pub fn failures(&self) -> u32 {
        self.failures
    }
}

/// The oldest entries that fit one batch: at most [`MAX_BATCH_REPORTS`] and
/// [`MAX_BATCH_BYTES`] serialized. Returns the batch, its JSON, and how many
/// queue entries it covers. `None` for an empty queue.
#[must_use]
pub fn next_batch(entries: &[QueueEntry], batch_id: BatchId) -> Option<(String, usize)> {
    let mut batch = TelemetryBatch {
        schema_version: TELEMETRY_SCHEMA_VERSION,
        batch_id,
        reports: Vec::new(),
    };
    // Envelope size, then each report plus its separating comma.
    let mut size = serde_json::to_string(&batch).ok()?.len();
    for e in entries.iter().take(MAX_BATCH_REPORTS) {
        let len = serde_json::to_string(&e.report).ok()?.len() + 1;
        if size + len > MAX_BATCH_BYTES && !batch.reports.is_empty() {
            break;
        }
        size += len;
        batch.reports.push(e.report.clone());
    }
    let n = batch.reports.len();
    let json = batch.to_json().ok()?;
    Some((json, n))
}

/// The outcome of one upload attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attempt {
    /// Nothing queued.
    Empty,
    /// This many reports left the queue (delivered or rejected).
    Sent(Delivery, usize),
    /// Kept for a retry.
    Kept,
}

/// Uploads from one queue, reusing a batch id while retrying the same batch
/// so the server can drop duplicates.
#[derive(Debug, Default)]
pub struct Uploader {
    /// `(id, first window start, report count)` of the batch last tried.
    pending: Option<(BatchId, u64, usize)>,
}

impl Uploader {
    /// A fresh uploader.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Send the oldest batch once.
    ///
    /// # Errors
    /// A queue read or write failure.
    pub async fn upload_once<T: Transport>(
        &mut self,
        queue: &DiskQueue,
        transport: &T,
        now_s: u64,
    ) -> Result<Attempt, QueueError> {
        let entries = queue.load()?;
        let Some(first) = entries.first() else {
            return Ok(Attempt::Empty);
        };
        let first_start = first.report.window.start_unix_s;
        // Packing does not depend on the id (fixed length), so size it first.
        let Some((_, n)) = next_batch(&entries, BatchId::generate()) else {
            // A report that cannot be serialized can never be sent.
            queue.remove_oldest(1, now_s)?;
            return Ok(Attempt::Sent(Delivery::Rejected, 1));
        };
        let id = match self.pending {
            Some((id, start, m)) if start == first_start && m == n => id,
            _ => BatchId::generate(),
        };
        let Some((json, n)) = next_batch(&entries, id) else {
            return Ok(Attempt::Kept);
        };
        self.pending = Some((id, first_start, n));
        let delivery = match transport.post(json, id.to_uuid_string()).await {
            Ok(status) => Delivery::of_status(status),
            Err(_) => Delivery::Retry,
        };
        if delivery == Delivery::Retry {
            return Ok(Attempt::Kept);
        }
        self.pending = None;
        queue.remove_oldest(n, now_s)?;
        Ok(Attempt::Sent(delivery, n))
    }
}

#[cfg(test)]
#[allow(clippy::assert_is_empty)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use crate::queue::QueueLimits;
    use crate::queue::tests::{report, temp_queue};

    const T: u64 = 1_750_000_200;

    struct Fake {
        statuses: RefCell<Vec<Result<u16, String>>>,
        seen: RefCell<Vec<(String, String)>>,
    }

    impl Fake {
        fn new(statuses: Vec<Result<u16, String>>) -> Self {
            Self {
                statuses: RefCell::new(statuses),
                seen: RefCell::new(Vec::new()),
            }
        }
    }

    impl Transport for Fake {
        fn post(&self, body: String, key: String) -> impl Future<Output = Result<u16, String>> {
            self.seen.borrow_mut().push((body, key));
            std::future::ready(self.statuses.borrow_mut().remove(0))
        }
    }

    fn block_on<F: Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(f)
    }

    #[test]
    fn statuses_are_classified() {
        assert_eq!(Delivery::of_status(202), Delivery::Accepted);
        assert_eq!(Delivery::of_status(200), Delivery::Accepted);
        for s in [400, 413, 422] {
            assert_eq!(Delivery::of_status(s), Delivery::Rejected, "{s}");
        }
        for s in [404, 408, 429, 500, 503] {
            assert_eq!(Delivery::of_status(s), Delivery::Retry, "{s}");
        }
    }

    #[test]
    fn backoff_doubles_with_jitter_and_caps() {
        let mut b = Backoff::new(Duration::from_secs(60), Duration::from_secs(3600));
        assert_eq!(b.fail(1.0), Duration::from_secs(60));
        assert_eq!(b.fail(1.0), Duration::from_secs(120));
        assert_eq!(b.fail(0.0), Duration::from_secs(120), "lower half of 240");
        for _ in 0..40 {
            let d = b.fail(0.99);
            assert!(d <= Duration::from_secs(3600), "{d:?}");
        }
        assert!(b.fail(0.0) >= Duration::from_secs(1800));
        assert_eq!(b.failures(), 44);
        b.reset();
        assert_eq!(b.fail(1.0), Duration::from_secs(60));
    }

    #[test]
    fn batches_respect_the_size_cap() {
        let entries: Vec<QueueEntry> = (0..400)
            .map(|i| QueueEntry {
                queued_unix_s: T,
                report: report(T + i * 300),
            })
            .collect();
        let (json, n) = next_batch(&entries, BatchId::generate()).unwrap();
        assert!(json.len() <= MAX_BATCH_BYTES, "{}", json.len());
        assert!(n > 0 && n <= MAX_BATCH_REPORTS);
        let back = TelemetryBatch::from_json(&json).unwrap();
        assert_eq!(back.reports.len(), n);
        assert_eq!(back.reports[0].window.start_unix_s, T, "oldest first");
        assert!(next_batch(&[], BatchId::generate()).is_none());
    }

    #[test]
    fn accepted_drains_unavailable_keeps_and_retries_reuse_the_id() {
        let (q, dir) = temp_queue("upload", QueueLimits::DEFAULT);
        q.push(report(T), T).unwrap();
        q.push(report(T + 300), T + 300).unwrap();
        let fake = Fake::new(vec![Ok(503), Err("timed out".into()), Ok(202)]);
        let mut up = Uploader::new();
        assert_eq!(
            block_on(up.upload_once(&q, &fake, T)).unwrap(),
            Attempt::Kept
        );
        assert_eq!(q.load().unwrap().len(), 2, "503 keeps the batch");
        assert_eq!(
            block_on(up.upload_once(&q, &fake, T)).unwrap(),
            Attempt::Kept
        );
        assert_eq!(
            block_on(up.upload_once(&q, &fake, T)).unwrap(),
            Attempt::Sent(Delivery::Accepted, 2)
        );
        assert!(q.load().unwrap().is_empty(), "202 drains the queue");
        let seen = fake.seen.borrow();
        assert_eq!(seen.len(), 3);
        assert!(
            seen.iter().all(|(_, k)| *k == seen[0].1),
            "same idempotency key"
        );
        assert_eq!(
            block_on(up.upload_once(&q, &fake, T)).unwrap(),
            Attempt::Empty
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_rejected_batch_is_dropped_not_retried_forever() {
        let (q, dir) = temp_queue("rejected", QueueLimits::DEFAULT);
        q.push(report(T), T).unwrap();
        let fake = Fake::new(vec![Ok(422)]);
        let mut up = Uploader::new();
        assert_eq!(
            block_on(up.upload_once(&q, &fake, T)).unwrap(),
            Attempt::Sent(Delivery::Rejected, 1)
        );
        assert!(q.load().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }
}
