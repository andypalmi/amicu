# Vendored: Modal control-plane proto

- Upstream repository: <https://github.com/modal-labs/modal-client>
- Tag: `v1.3.1`
- Commit: `2a31e94ae4654566753a6dd2125af64eb9a11e77`
- Vendored on: 2026-09-18
- License: Apache-2.0 (see `LICENSE` in this directory, copied unmodified
  from the upstream repository at the pinned commit)

## Files

- `modal_proto/api.proto` — copied byte-for-byte from
  `modal_proto/api.proto` at the pinned commit. Only standard
  `google/protobuf/*.proto` well-known types are imported (`any`, `empty`,
  `struct`, `timestamp`, `wrappers`); the upstream checkout at this tag has
  no `modal_proto/options.proto` or other custom-option proto to vendor
  alongside it, and `api.proto` defines no custom field/message options of
  its own (checked via `grep -n "extend \|options.proto"`).

`task_command_router.proto` (the exec/stdin command-router service) is
intentionally **not** vendored — this PR only needs `ClientHello` from the
`ModalClient` service in `api.proto`, and the router service is out of
scope per `andypalmi/amicu-planning#158`.

## Drift check

CI should diff this file against the pinned tag/commit above (same pattern
as `andypalmi/amicu-planning#156`) so an upstream proto change is caught
rather than silently drifting. Bumping the pin is a deliberate, reviewed
change: update the tag/commit/date here, re-copy `api.proto`, and re-run
`cargo build -p amicu-modal` to confirm the vendored file still compiles
with `tonic-build`/`prost-build`.
