//! Advisory lock on the project's local state (amicu-planning#178).
//!
//! Two `amicu apply` runs (or an apply and `amicu modal stop` pruning state)
//! would otherwise interleave their read-modify-write of
//! `.amicu/state/state.json` and the last writer would silently win. The lock
//! is an OS advisory lock on `.amicu/state/state.lock`, so the kernel releases
//! it when the process dies: there is no stale lock to clean up.
//!
//! The lock file's contents (pid, command, start time) are informational only,
//! used to make the "locked" error actionable; the OS lock is the source of
//! truth. `File` locks are per open file description, so two handles in one
//! process conflict, which the tests rely on.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{Seek, Write};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::CliError;

/// How often a waiting acquirer retries (`--lock-timeout`).
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// What the holder wrote into the lock file after acquiring it.
#[derive(Debug, Serialize, Deserialize)]
struct Holder {
    pid: u32,
    command: String,
    /// Seconds since the Unix epoch.
    started_unix: u64,
}

impl Holder {
    fn describe(&self) -> String {
        format!(
            "pid {}, {}, started {}",
            self.pid,
            self.command,
            format_time_of_day(self.started_unix)
        )
    }
}

/// `HH:MM:SS UTC` for a Unix timestamp.
fn format_time_of_day(unix: u64) -> String {
    let day = unix % 86_400;
    format!(
        "{:02}:{:02}:{:02} UTC",
        day / 3_600,
        day % 3_600 / 60,
        day % 60
    )
}

/// A held state lock; released when dropped (the OS also releases it if the
/// process dies).
#[derive(Debug)]
pub(crate) struct StateLock {
    file: File,
}

impl StateLock {
    /// Acquires the lock in `state_dir` (created if missing) on behalf of
    /// `command`. With `timeout == None` it fails fast when the lock is held;
    /// otherwise it retries until the timeout elapses.
    pub(crate) fn acquire(
        state_dir: &Path,
        command: &str,
        timeout: Option<Duration>,
    ) -> Result<Self, CliError> {
        fs::create_dir_all(state_dir)
            .map_err(|e| CliError::io(format!("cannot create {}", state_dir.display()), e))?;
        let path = state_dir.join("state.lock");
        // Never truncate on open: the current holder's info lives in this file
        // and must stay readable until we actually hold the lock.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| CliError::io(format!("cannot open {}", path.display()), e))?;

        let deadline = timeout.map(|t| Instant::now() + t);
        loop {
            match file.try_lock() {
                Ok(()) => break,
                Err(TryLockError::WouldBlock) => {
                    if deadline.is_none_or(|d| Instant::now() >= d) {
                        return Err(locked_error(&path, timeout));
                    }
                    std::thread::sleep(POLL_INTERVAL);
                }
                Err(TryLockError::Error(e)) => {
                    return Err(CliError::io(format!("cannot lock {}", path.display()), e));
                }
            }
        }

        let mut lock = Self { file };
        lock.write_holder(command)
            .map_err(|e| CliError::io(format!("cannot write {}", path.display()), e))?;
        Ok(lock)
    }

    fn write_holder(&mut self, command: &str) -> std::io::Result<()> {
        let holder = Holder {
            pid: std::process::id(),
            command: command.to_owned(),
            started_unix: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
        };
        let text = serde_json::to_string(&holder).map_err(std::io::Error::other)?;
        // Safe to truncate now: we hold the lock.
        self.file.set_len(0)?;
        self.file.rewind()?;
        self.file.write_all(text.as_bytes())?;
        self.file.flush()
    }
}

impl Drop for StateLock {
    fn drop(&mut self) {
        // Best effort; closing the file releases the lock regardless.
        let _ = self.file.unlock();
    }
}

/// Builds the "locked" error, naming the holder when its info is readable.
fn locked_error(path: &Path, timeout: Option<Duration>) -> CliError {
    let holder = fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Holder>(&text).ok())
        .map_or_else(|| "holder unknown".to_owned(), |h| h.describe());
    let advice = match timeout {
        None => "retry, or use --lock-timeout".to_owned(),
        Some(t) => format!("gave up after {}s; retry later", t.as_secs()),
    };
    CliError::StateLocked(format!(
        "state is locked by another amicu process ({holder}); {advice}"
    ))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    /// A unique scratch project directory, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            Self(std::env::temp_dir().join(format!("amicu-lock-{}-{n}", std::process::id())))
        }

        /// The (not yet created) state directory.
        fn state_dir(&self) -> PathBuf {
            self.0.join(".amicu").join("state")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn second_acquirer_fails_fast_while_held() {
        let s = Scratch::new();
        let _held = StateLock::acquire(&s.state_dir(), "apply", None).unwrap();
        let started = Instant::now();
        let err = StateLock::acquire(&s.state_dir(), "apply", None).unwrap_err();
        assert!(matches!(err, CliError::StateLocked(_)), "{err}");
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn acquires_after_release() {
        let s = Scratch::new();
        drop(StateLock::acquire(&s.state_dir(), "apply", None).unwrap());
        StateLock::acquire(&s.state_dir(), "apply", None).unwrap();
    }

    #[test]
    fn creates_the_state_dir() {
        let s = Scratch::new();
        assert!(!s.state_dir().exists());
        let _lock = StateLock::acquire(&s.state_dir(), "apply", None).unwrap();
        assert!(s.state_dir().join("state.lock").is_file());
    }

    #[test]
    fn error_names_the_holder() {
        let s = Scratch::new();
        let _held = StateLock::acquire(&s.state_dir(), "apply", None).unwrap();
        let message = StateLock::acquire(&s.state_dir(), "modal stop", None)
            .unwrap_err()
            .to_string();
        assert!(
            message.contains(&format!("pid {}", std::process::id())),
            "{message}"
        );
        assert!(message.contains("apply"), "{message}");
        assert!(message.contains("started"), "{message}");
        assert!(message.contains("--lock-timeout"), "{message}");
    }

    #[test]
    fn failed_acquire_does_not_clobber_holder_info() {
        let s = Scratch::new();
        let _held = StateLock::acquire(&s.state_dir(), "apply", None).unwrap();
        let _ = StateLock::acquire(&s.state_dir(), "modal stop", None).unwrap_err();
        let text = fs::read_to_string(s.state_dir().join("state.lock")).unwrap();
        assert!(text.contains("apply"), "{text}");
    }

    #[test]
    fn timeout_waits_for_a_release() {
        let s = Scratch::new();
        let held = StateLock::acquire(&s.state_dir(), "apply", None).unwrap();
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            drop(held);
        });
        let started = Instant::now();
        StateLock::acquire(&s.state_dir(), "apply", Some(Duration::from_secs(10))).unwrap();
        assert!(started.elapsed() >= Duration::from_millis(250));
        releaser.join().unwrap();
    }

    #[test]
    fn timeout_gives_up_when_still_held() {
        let s = Scratch::new();
        let _held = StateLock::acquire(&s.state_dir(), "apply", None).unwrap();
        let started = Instant::now();
        let err = StateLock::acquire(&s.state_dir(), "apply", Some(Duration::from_millis(300)))
            .unwrap_err()
            .to_string();
        assert!(started.elapsed() >= Duration::from_millis(250));
        assert!(err.contains("gave up"), "{err}");
        assert!(err.contains("pid"), "{err}");
    }
}
