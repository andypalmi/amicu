//! The command implementations.

pub(crate) mod apply;
pub(crate) mod catalog;
pub(crate) mod destroy;
pub(crate) mod doctor;
pub(crate) mod fmt;
pub(crate) mod init;
pub(crate) mod modal;
pub(crate) mod orphans;
pub(crate) mod plan;
pub(crate) mod skill;
pub(crate) mod telemetry;
pub(crate) mod telemetry_agent;
pub(crate) mod validate;

/// A fresh current-thread Tokio runtime with every driver (IO, time) enabled.
/// Shared by every command that reaches network code (`apply`, `modal ...`):
/// a bare `Builder::new_current_thread().build()` has no IO driver, so the
/// first socket panics with "IO is disabled".
pub(crate) fn build_runtime() -> Result<tokio::runtime::Runtime, crate::error::CliError> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| crate::error::CliError::io("cannot start async runtime", e))
}

#[cfg(test)]
mod tests {
    use super::build_runtime;

    /// Regression: the `apply` runtime lacked the IO driver, so the Modal
    /// client panicked on its first socket. `#[tokio::test]` hides this by
    /// enabling everything, so build the real helper and touch the network.
    #[test]
    fn build_runtime_has_io_and_time_drivers() {
        let rt = build_runtime().expect("runtime");
        rt.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind needs the IO driver");
            let addr = listener.local_addr().unwrap();
            let (client, accepted) =
                tokio::join!(tokio::net::TcpStream::connect(addr), listener.accept());
            client.expect("connect");
            accepted.expect("accept");
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        });
    }
}
