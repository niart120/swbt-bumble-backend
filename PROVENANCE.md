# Source provenance

`swbt-bumble-backend` combines modified portions of `bumble-rs` with new
backend-specific code. It is not an unmodified copy of a single upstream
package.

## Revision chain

The upstream source baseline is
[`chaitanyarahalkar/bumble-rs@bbac2a6`](https://github.com/chaitanyarahalkar/bumble-rs/commit/bbac2a6803b8cab0920ab725a23aa408fc4fed85).

The intermediate [`niart120/bumble-rs`](https://github.com/niart120/bumble-rs)
fork adds three changes required by `swbt-rs`:

| revision | change | affected source |
|---|---|---|
| `48f1bc36169b2692d2a61e87eda4223b126dca2b` | reader shutdown lifecycle | `bumble-transport` |
| `b8c7cd625bc2ac2f58a4beb4ade1264426969819` | observable host-side ACL flush state | `bumble-host`, `bumble-transport` |
| `cb55e2d98dc7b7b0227c43772c9ae184034dd9a1` | vendor command responses | `bumble-transport` |

The standalone crate was extracted from the resulting fork revision
`cb55e2d98dc7b7b0227c43772c9ae184034dd9a1`. Its repository retains the
subdirectory history for the extraction and later backend-specific changes.

## Bumble-derived source mapping

All source paths in this table refer to the upstream baseline unless stated
otherwise.

| distributed path | source path or behavior source | modification summary |
|---|---|---|
| `src/values.rs` | `bumble/src/address.rs`, `bumble/src/keys.rs`, `bumble/src/uuid.rs` | combines the retained value types, removes LE key material and catalogs, adds owned public types and redacted debug output |
| `src/hci.rs` | `bumble-hci/src/codes.rs`, `command.rs`, `event.rs`, `packet.rs`, `return_parameters.rs`, `metadata.rs`, `metadata_tables.rs` | reduces the generated surface to required command, event, and ACL forms; rejects SCO and ISO packets; adds strict framing checks |
| `src/usb.rs` | `bumble-transport/src/common.rs`, `usb.rs`, `dispatch.rs`, `command_channel.rs`, `host.rs` | keeps USB command/event/ACL transport, adds bounded reader cancellation and join, removes generic dispatch, SCO, and direct `libusb1-sys` use; incorporates the fork reader and vendor-response changes |
| `src/classic_host.rs` | `bumble-host/src/lib.rs`, `bumble-host/src/data_queue.rs`, `bumble-transport/src/host.rs` | rewrites the host as Classic-only state, removes LE/GATT/SMP state, and incorporates the fork ACL flush observation |
| `src/l2cap/mod.rs` | `bumble-l2cap/src/lib.rs` | reduces the codec and signaling surface to Classic L2CAP |
| `src/l2cap/classic.rs` | `bumble-l2cap/src/classic.rs` | rewrites Classic channel state as synchronous sans-I/O logic and removes unrelated bindings |
| `src/l2cap/ertm.rs` | `bumble-l2cap/src/ertm.rs` | retains required negotiation and retransmission behavior behind the Classic-only boundary |
| `src/sdp/mod.rs`, `src/sdp/pdu.rs`, `src/sdp/service.rs` | `bumble-sdp/src/lib.rs`, `pdu.rs`, `service.rs` | internalizes the SDP codec and synchronous service runtime; removes the upstream L2CAP binding |
| `src/hidp.rs` | `bumble-hid/src/lib.rs` | internalizes HIDP messages and device dispatch while leaving L2CAP ownership with the backend session |
| `src/l2cap/classic_tests.rs`, `src/l2cap/ertm_tests.rs` | `bumble-l2cap` Classic and ERTM tests | ports and extends the behavior fixtures for the rewritten sans-I/O implementation |
| `src/sdp/service_tests.rs` | `bumble-sdp/tests/service.rs` and upstream response behavior | ports continuation and request/response fixtures to the internal service runtime |
| `src/hidp/tests.rs` | `bumble-hid/tests/protocol.rs` | ports message and dispatch fixtures to the internal HIDP implementation |

## swbt-rs-derived and new source

The following files are not direct copies of the upstream Bumble packages:

| distributed path | origin | modification summary |
|---|---|---|
| `src/csr.rs`, `src/identity.rs` | [`niart120/swbt-rs@b61476f`](https://github.com/niart120/swbt-rs/commit/b61476f1320906e3b01af1f5e49f832d9740741f) | moves the CSR volatile-address and recovery state into the backend and adapts it to the private HCI and USB boundaries |
| `src/hid_service.rs` | [`niart120/swbt-rs@a36a69b`](https://github.com/niart120/swbt-rs/commit/a36a69bd266904f4f2ce52ef0d787b3793ddaf5d) | moves the HID service policy into the backend and adapts it to the internal SDP and HIDP implementations |
| `src/session.rs` | `swbt-rs` transport behavior and the Bumble-derived modules listed above | introduces the owned session, event queue, pairing and reconnect orchestration, flow control, and ordered shutdown |
| `src/api.rs`, `src/lib.rs`, `tests/public_api.rs` | new in `swbt-bumble-backend` | defines and verifies the backend-owned public boundary without exposing Bumble protocol types |

## License and notices

The distributed `LICENSE` is the Apache License, Version 2.0 copy retained
from the upstream baseline. `NOTICE` reproduces the upstream attribution and
adds the intermediate-fork and standalone modification notices. Modified and
ported files identify their lineage and the nature of their changes near the
start of each file.
