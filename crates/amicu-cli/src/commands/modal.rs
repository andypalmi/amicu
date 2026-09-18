//! `amicu modal auth-check` - authenticate against Modal's control plane.
//!
//! Project-independent (like `skill export`): it needs no `*.hcl` config,
//! just `MODAL_TOKEN_ID`/`MODAL_TOKEN_SECRET` in the environment.

use std::io::Write;

use amicu_modal::{Credentials, ModalClient, ModalError};

use crate::error::CliError;

/// Connects to Modal and calls `ClientHello`, the minimal RPC that
/// confirms the configured token is accepted. Exits non-zero on any
/// failure (missing/invalid credentials, connect failure, or a non-OK
/// gRPC status).
pub(crate) fn auth_check(out: &mut dyn Write) -> Result<u8, CliError> {
    let credentials = Credentials::from_env().map_err(ModalError::from)?;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| CliError::io("cannot start async runtime", e))?;

    let hello = rt.block_on(async {
        let mut client = ModalClient::connect(credentials).await?;
        client.hello().await
    })?;

    // ClientHelloResponse carries no workspace/server identity field at the
    // vendored proto pin (only `warning`/`server_warnings`) — print those
    // if present, otherwise the plain confirmation the task asks for.
    writeln!(out, "authenticated").map_err(|e| CliError::io("write failed", e))?;
    if let Some(warning) = &hello.warning {
        writeln!(out, "  warning: {warning}").map_err(|e| CliError::io("write failed", e))?;
    }
    for server_warning in &hello.server_warnings {
        writeln!(out, "  server warning: {server_warning}")
            .map_err(|e| CliError::io("write failed", e))?;
    }
    Ok(0)
}
