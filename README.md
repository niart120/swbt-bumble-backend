# swbt-bumble-backend

`swbt-bumble-backend` is the Bluetooth Classic HID backend extracted for
[`swbt-rs`](https://github.com/niart120/swbt-rs).

This is an implementation-oriented backend, not a general replacement for
Bumble. Its public API owns a Bluetooth Classic HID session without exposing
the internal HCI, L2CAP, SDP, or HIDP protocol types.

The crate contains:

- USB HCI transport and the command, event, and ACL packet subset used by
  `swbt-rs`;
- Bluetooth Classic pairing, stored-bond reconnect, Classic L2CAP, SDP, and
  HIDP state;
- bounded application events, interrupt-report flow control, and ordered
  session shutdown.

LE GATT/ATT/SMP, audio profiles, RFCOMM, serial, WebSocket, and gRPC
transports are outside this crate's scope. The API may change before version
1.0 as the `swbt-rs` integration is completed.

API documentation is published at
[`docs.rs/swbt-bumble-backend`](https://docs.rs/swbt-bumble-backend) for each
crates.io release. Supported Rust versions are declared by `rust-version` in
`Cargo.toml` and checked in CI.

## Provenance

The upstream source baseline is
[`chaitanyarahalkar/bumble-rs@bbac2a6`](https://github.com/chaitanyarahalkar/bumble-rs/commit/bbac2a6803b8cab0920ab725a23aa408fc4fed85).
That source received three `swbt-rs`-required host and transport changes in
[`niart120/bumble-rs`](https://github.com/niart120/bumble-rs) through revision
`cb55e2d98dc7b7b0227c43772c9ae184034dd9a1`. This crate was then extracted
from that fork revision and further modified as a standalone backend.

[PROVENANCE.md](PROVENANCE.md) maps the derived files to their source paths
and summarizes the changes. The Apache-2.0 license, upstream attribution, and
modification notices are retained in `LICENSE`, `NOTICE`, and the derived
source files.

## Security

Report security-sensitive defects through this repository's
[private vulnerability reporting](https://github.com/niart120/swbt-bumble-backend/security/advisories/new)
instead of a public issue. See [SECURITY.md](SECURITY.md) for the supported
release policy.

## License

Licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE) and
[NOTICE](NOTICE).
