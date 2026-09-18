//! Live integration test: a real `ClientHello` RPC against Modal.
//!
//! `#[ignore]`d because it needs network access and a real Modal token, and
//! this crate's other tests (interceptor headers, status-code mapping) are
//! deliberately network-free. To run it:
//!
//! ```sh
//! MODAL_TOKEN_ID=ak-... MODAL_TOKEN_SECRET=as-... \
//!     cargo test -p amicu-modal --test live_hello -- --ignored
//! ```
//!
//! Point `AMICU_MODAL_ENDPOINT` at a different endpoint (e.g. a local mock
//! server) to avoid hitting the real Modal API.

use amicu_modal::{Credentials, ModalClient};

#[tokio::test]
#[ignore = "needs MODAL_TOKEN_ID/MODAL_TOKEN_SECRET and network access; see module docs"]
async fn client_hello_against_the_real_modal_api() {
    let credentials = Credentials::from_env()
        .expect("MODAL_TOKEN_ID and MODAL_TOKEN_SECRET must both be set to run this test");

    let mut client = ModalClient::connect(credentials)
        .await
        .expect("connect to Modal");
    let hello = client.hello().await.expect("ClientHello RPC");

    println!("ClientHello ok; clean={}", hello.is_clean());
    if let Some(warning) = &hello.warning {
        println!("warning: {warning}");
    }
    for server_warning in &hello.server_warnings {
        println!("server warning: {server_warning}");
    }
}
