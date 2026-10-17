//! The agent's bounded on-disk buffer of reports waiting to be uploaded.
//!
//! One JSON-lines file per deployment, owned by that deployment's agent, so
//! agents never contend for a file. It survives restarts and is bounded three
//! ways, always dropping the **oldest** entries first:
//!
//! - entries older than [`QueueLimits::max_age_s`] (the server keeps 5-minute
//!   rollups for 7 days, so anything older is useless),
//! - more than [`QueueLimits::max_entries`] entries,
//! - a file larger than [`QueueLimits::max_bytes`].
//!
//! While the ingestion endpoint is unreachable the file fills up to the cap
//! and then holds the newest data only. It never grows without bound. Writes
//! go to a private (0600) temp file that is renamed over the old one, so a
//! crash never leaves a half-written queue; unreadable lines are skipped.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::report::TelemetryReport;

/// The caps on one queue file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueLimits {
    pub max_bytes: usize,
    pub max_entries: usize,
    pub max_age_s: u64,
}

impl QueueLimits {
    /// 7 days of 5-minute windows, in at most 512 KiB.
    pub const DEFAULT: Self = Self {
        max_bytes: 512 * 1024,
        max_entries: 7 * 24 * 12,
        max_age_s: 7 * 24 * 3600,
    };
}

/// One queued report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueueEntry {
    /// When the report was queued, seconds since the epoch.
    pub queued_unix_s: u64,
    pub report: TelemetryReport,
}

/// A queue I/O failure (the path and the OS reason; never report content).
#[derive(Debug)]
pub struct QueueError(pub String);

impl std::fmt::Display for QueueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "telemetry queue: {}", self.0)
    }
}

impl std::error::Error for QueueError {}

/// The bounded queue file at a fixed path.
#[derive(Debug, Clone)]
pub struct DiskQueue {
    path: PathBuf,
    limits: QueueLimits,
}

impl DiskQueue {
    /// The queue at `path` (created on first write).
    pub fn new(path: impl Into<PathBuf>, limits: QueueLimits) -> Self {
        Self {
            path: path.into(),
            limits,
        }
    }

    /// The file path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Every readable entry, oldest first. A missing file is an empty queue.
    ///
    /// # Errors
    /// A read failure other than "not found".
    pub fn load(&self) -> Result<Vec<QueueEntry>, QueueError> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(self.err("read", &e)),
        };
        Ok(text
            .lines()
            .filter_map(|l| serde_json::from_str::<QueueEntry>(l).ok())
            .filter(|e| e.report.validate().is_ok())
            .collect())
    }

    /// Append `report` queued at `now_s`, then enforce the caps. Returns how
    /// many entries were dropped to make it fit.
    ///
    /// # Errors
    /// A read or write failure.
    pub fn push(&self, report: TelemetryReport, now_s: u64) -> Result<usize, QueueError> {
        let mut entries = self.load()?;
        entries.push(QueueEntry {
            queued_unix_s: now_s,
            report,
        });
        let before = entries.len();
        let kept = self.bound(entries, now_s);
        let dropped = before - kept.len();
        self.store(&kept)?;
        Ok(dropped)
    }

    /// Drop the `n` oldest entries (they were uploaded) and expire old ones.
    ///
    /// # Errors
    /// A read or write failure.
    pub fn remove_oldest(&self, n: usize, now_s: u64) -> Result<(), QueueError> {
        let entries = self.load()?;
        let rest = entries.into_iter().skip(n).collect();
        let kept = self.bound(rest, now_s);
        self.store(&kept)
    }

    /// Delete the queue file.
    ///
    /// # Errors
    /// A delete failure other than "not found".
    pub fn clear(&self) -> Result<(), QueueError> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(self.err("remove", &e)),
        }
    }

    /// Apply the age, count and size caps, dropping oldest first.
    fn bound(&self, mut entries: Vec<QueueEntry>, now_s: u64) -> Vec<QueueEntry> {
        entries.retain(|e| now_s.saturating_sub(e.queued_unix_s) <= self.limits.max_age_s);
        if entries.len() > self.limits.max_entries {
            entries.drain(..entries.len() - self.limits.max_entries);
        }
        let sizes: Vec<usize> = entries.iter().map(line_len).collect();
        let mut total: usize = sizes.iter().sum();
        let mut skip = 0;
        while total > self.limits.max_bytes && skip < sizes.len() {
            total -= sizes[skip];
            skip += 1;
        }
        entries.drain(..skip);
        entries
    }

    fn store(&self, entries: &[QueueEntry]) -> Result<(), QueueError> {
        if entries.is_empty() {
            return self.clear();
        }
        let dir = self
            .path
            .parent()
            .ok_or_else(|| QueueError("queue path has no directory".to_owned()))?;
        ensure_private_dir(dir).map_err(|e| self.err("create directory for", &e))?;
        let mut body = String::new();
        for e in entries {
            if let Ok(line) = serde_json::to_string(e) {
                body.push_str(&line);
                body.push('\n');
            }
        }
        let tmp = self.path.with_extension("tmp");
        let _ = std::fs::remove_file(&tmp);
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let written = opts
            .open(&tmp)
            .and_then(|mut f| f.write_all(body.as_bytes()).and_then(|()| f.sync_all()));
        if let Err(e) = written.and_then(|()| std::fs::rename(&tmp, &self.path)) {
            let _ = std::fs::remove_file(&tmp);
            return Err(self.err("write", &e));
        }
        Ok(())
    }

    fn err(&self, what: &str, e: &std::io::Error) -> QueueError {
        QueueError(format!("cannot {what} {}: {e}", self.path.display()))
    }
}

fn line_len(e: &QueueEntry) -> usize {
    serde_json::to_string(e).map_or(0, |s| s.len() + 1)
}

/// Create `dir` and its parents, the leaf owner-only (0700) on unix.
///
/// # Errors
/// The OS error.
pub fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}

#[cfg(test)]
#[allow(clippy::assert_is_empty)]
pub(crate) mod tests {
    use super::*;
    use crate::identity::{AnonDeploymentId, InstallId};
    use crate::report::{Label, ModelInfo, Rollup, RuntimeInfo, TELEMETRY_SCHEMA_VERSION, Window};

    pub(crate) fn report(start: u64) -> TelemetryReport {
        TelemetryReport {
            schema_version: TELEMETRY_SCHEMA_VERSION,
            install_id: InstallId::from_random_bytes([1; 16]),
            deployment_id: AnonDeploymentId::from_random_bytes([2; 16]),
            window: Window {
                start_unix_s: start,
                duration_s: 300,
            },
            amicu_version: Label::new("0.4.0").unwrap(),
            hardware: crate::report::HardwareInfo::default(),
            model: ModelInfo::new(Label::new("custom").unwrap()),
            runtime: RuntimeInfo::new(Label::new("llama.cpp").unwrap()),
            provider: None,
            region: None,
            context_length: Some(4096),
            batch_size: None,
            metrics: Rollup {
                output_tokens: 10,
                ..Rollup::default()
            },
        }
    }

    pub(crate) fn temp_queue(tag: &str, limits: QueueLimits) -> (DiskQueue, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "amicu-queue-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        (DiskQueue::new(dir.join("q/d.jsonl"), limits), dir)
    }

    const T: u64 = 1_750_000_200;

    #[test]
    fn survives_reopen_and_keeps_order() {
        let (q, dir) = temp_queue("reopen", QueueLimits::DEFAULT);
        assert!(q.load().unwrap().is_empty());
        q.push(report(T), T).unwrap();
        q.push(report(T + 300), T + 300).unwrap();
        let reopened = DiskQueue::new(q.path(), QueueLimits::DEFAULT);
        let starts: Vec<u64> = reopened
            .load()
            .unwrap()
            .iter()
            .map(|e| e.report.window.start_unix_s)
            .collect();
        assert_eq!(starts, vec![T, T + 300]);
        reopened.remove_oldest(1, T + 300).unwrap();
        assert_eq!(reopened.load().unwrap().len(), 1);
        reopened.remove_oldest(1, T + 300).unwrap();
        assert!(!q.path().exists(), "an empty queue leaves no file");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn count_cap_drops_oldest_first() {
        let limits = QueueLimits {
            max_entries: 3,
            ..QueueLimits::DEFAULT
        };
        let (q, dir) = temp_queue("count", limits);
        let mut dropped = 0;
        for i in 0..5 {
            dropped += q.push(report(T + i * 300), T + i * 300).unwrap();
        }
        assert_eq!(dropped, 2);
        let starts: Vec<u64> = q
            .load()
            .unwrap()
            .iter()
            .map(|e| e.report.window.start_unix_s)
            .collect();
        assert_eq!(starts, vec![T + 600, T + 900, T + 1200]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn size_cap_bounds_the_file_while_offline() {
        let line = line_len(&QueueEntry {
            queued_unix_s: T,
            report: report(T),
        });
        let limits = QueueLimits {
            max_bytes: line * 4,
            ..QueueLimits::DEFAULT
        };
        let (q, dir) = temp_queue("size", limits);
        // The endpoint never answers: a week of windows piles up.
        for i in 0..200 {
            q.push(report(T + i * 300), T + i * 300).unwrap();
            let size = std::fs::metadata(q.path()).unwrap().len();
            assert!(size <= (line * 4) as u64, "file grew to {size}");
        }
        let entries = q.load().unwrap();
        assert_eq!(entries.len(), 4);
        assert_eq!(
            entries.last().unwrap().report.window.start_unix_s,
            T + 199 * 300
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn age_cap_expires_stale_entries() {
        let (q, dir) = temp_queue("age", QueueLimits::DEFAULT);
        q.push(report(T), T).unwrap();
        let week = QueueLimits::DEFAULT.max_age_s;
        q.push(report(T + week), T + week + 1).unwrap();
        let entries = q.load().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].report.window.start_unix_s, T + week);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn corrupt_lines_are_skipped_and_the_file_is_private() {
        let (q, dir) = temp_queue("corrupt", QueueLimits::DEFAULT);
        q.push(report(T), T).unwrap();
        let mut text = std::fs::read_to_string(q.path()).unwrap();
        text.push_str("{not json\n{\"queued_unix_s\":1,\"report\":{\"prompt\":\"x\"}}\n");
        std::fs::write(q.path(), text).unwrap();
        assert_eq!(q.load().unwrap().len(), 1);
        q.push(report(T + 300), T + 300).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(q.path()).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
            let dmode = std::fs::metadata(q.path().parent().unwrap())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(dmode & 0o777, 0o700);
        }
        q.clear().unwrap();
        q.clear().unwrap();
        assert!(q.load().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }
}
