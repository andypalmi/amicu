//! Shared helpers for the live (`#[ignore]`d) integration tests in this
//! crate. `tests/common/mod.rs` is not itself compiled as a separate test
//! binary (cargo only treats files directly under `tests/` that way), so
//! each test file that wants these helpers declares `mod common;` and gets
//! its own copy compiled in.
//!
//! Not every helper here is called directly by every test binary that
//! includes this module (e.g. one file may only reach [`connect`] through
//! [`AppGuard::install`]), so silence the resulting dead-code warning under
//! `clippy --all-targets` per binary rather than duplicating code to dodge
//! it.
#![allow(dead_code)]

use amicu_modal::{Credentials, ModalClient};

/// Connects using `MODAL_TOKEN_ID`/`MODAL_TOKEN_SECRET` from the
/// environment. Callers check `Credentials::from_env()` via
/// `have_credentials` first, so the `expect` here should never fire.
pub async fn connect() -> ModalClient {
    let credentials = Credentials::from_env().expect("checked by have_credentials");
    ModalClient::connect(credentials)
        .await
        .expect("connect to Modal")
}

/// Stops the named app on drop, however the test ends.
///
/// It holds its own `ModalClient` (a second channel to the same endpoint) so
/// cleanup does not depend on the client the test was using still being alive
/// or unborrowed, and it resolves the app by *name* so it also works when
/// `deploy_server` panicked before returning an id.
pub struct AppGuard {
    client: Option<ModalClient>,
    app_name: String,
}

impl AppGuard {
    pub async fn install(app_name: String) -> Self {
        Self {
            client: Some(connect().await),
            app_name,
        }
    }
}

/// Resolve the app by name and stop it, reporting whatever happens.
async fn stop_named_app(mut client: ModalClient, app_name: String) {
    match client.find_app(&app_name).await {
        Ok(Some(app_id)) => match client.stop_app(&app_id).await {
            Ok(()) => println!("cleanup: stopped {app_name} ({app_id})"),
            Err(error) => println!("cleanup: AppStop for {app_name} failed: {error}"),
        },
        Ok(None) => println!("cleanup: no app named {app_name} to stop"),
        Err(error) => println!("cleanup: lookup of {app_name} failed: {error}"),
    }
}

impl Drop for AppGuard {
    fn drop(&mut self) {
        let Some(client) = self.client.take() else {
            return;
        };
        let app_name = self.app_name.clone();

        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            println!("cleanup: no tokio runtime in Drop; {app_name} was NOT stopped");
            return;
        };

        // `block_in_place` is the only way to await from `Drop`, but it panics
        // on a current-thread runtime. These tests ask for `multi_thread`; if
        // that ever changes, degrade to a detached task rather than turning
        // cleanup into a second panic on the way out of a failing test.
        if matches!(
            handle.runtime_flavor(),
            tokio::runtime::RuntimeFlavor::MultiThread
        ) {
            tokio::task::block_in_place(|| handle.block_on(stop_named_app(client, app_name)));
        } else {
            println!(
                "cleanup: warning: not a multi-thread runtime, spawning a detached cleanup for \
                 {app_name}; it may not finish before the process exits"
            );
            handle.spawn(stop_named_app(client, app_name));
        }
    }
}
