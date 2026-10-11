//! Local transport: runs [`PROBE_SCRIPT`](super::PROBE_SCRIPT) on this host.
//!
//! Library function for now (a future `amicu metrics` for `apply`-managed
//! local deployments will call it). The script is the same fixed text the
//! Modal transport sends; this runs it as `sh -c <script>` with no stdin,
//! stderr discarded, stdout capped and a hard timeout.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::{HardwareSample, MAX_PROBE_OUTPUT_BYTES, PROBE_SCRIPT, parse_probe_output};

/// Why a local probe produced nothing.
#[derive(Debug)]
pub enum HardwareError {
    /// `sh` could not be started.
    Spawn(String),
    /// The probe did not finish within the timeout and was killed.
    TimedOut(Duration),
}

impl std::fmt::Display for HardwareError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(why) => write!(f, "could not start the hardware probe: {why}"),
            Self::TimedOut(after) => {
                write!(f, "the hardware probe timed out after {}s", after.as_secs())
            }
        }
    }
}

impl std::error::Error for HardwareError {}

/// Runs the probe on the local host and parses it.
pub fn probe_local(timeout: Duration) -> Result<HardwareSample, HardwareError> {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(PROBE_SCRIPT)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| HardwareError::Spawn(e.kind().to_string()))?;

    let stdout = child.stdout.take();
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(out) = stdout {
            // One byte past the cap is enough to know it was exceeded.
            let _ = out
                .take(MAX_PROBE_OUTPUT_BYTES as u64 + 1)
                .read_to_end(&mut buf);
        }
        buf
    });

    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(HardwareError::TimedOut(timeout));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return Err(HardwareError::Spawn(e.kind().to_string())),
        }
    }
    let bytes = reader.join().unwrap_or_default();
    Ok(parse_probe_output(&String::from_utf8_lossy(&bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_real_probe_runs_here_and_never_fabricates() {
        // Whatever this machine has, the probe must finish, parse, and keep
        // `None` for anything it could not read.
        let sample = probe_local(Duration::from_secs(20)).expect("probe runs");
        for a in &sample.accelerators {
            if let Some(util) = a.util_pct {
                assert!(util.is_finite() && util >= 0.0);
            }
        }
    }

    #[test]
    fn errors_render_without_detail_leaks() {
        assert!(
            HardwareError::TimedOut(Duration::from_secs(10))
                .to_string()
                .contains("10s")
        );
    }
}
