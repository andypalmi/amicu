//! Generated tonic/prost bindings for Modal's control-plane RPC service.
//!
//! Generated from `proto/modal_proto/api.proto` by `build.rs` (see
//! `proto/VENDORED.md` for the upstream pin). This module is intentionally
//! `pub(crate)`: it is Modal's full internal RPC surface (175 RPCs at the
//! pinned tag), most of it unrelated to this crate's public API, and
//! upstream's own proto header states direct gRPC use is unsupported and
//! may change without notice — nothing outside this crate should couple to
//! generated message/service types directly.
//!
//! Lints are blanket-allowed here (not fixed up) because this is verbatim
//! codegen output, not hand-written code.
#[allow(
    clippy::all,
    clippy::pedantic,
    clippy::nursery,
    unreachable_pub,
    dead_code
)]
pub(crate) mod modal_client {
    include!(concat!(env!("OUT_DIR"), "/modal.client.rs"));
}
