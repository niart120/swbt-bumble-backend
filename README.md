# swbt-bumble-backend

`swbt-bumble-backend` is the Bluetooth Classic HID backend being extracted for
[`swbt-rs`](https://github.com/niart120/swbt-rs). It is not published yet.

The package boundary is intentionally narrower than the Bumble workspace. It
will contain USB HCI, the required HCI command/event/ACL codecs, Bluetooth
Classic pairing and reconnect state, Classic L2CAP, SDP, HIDP, and ordered
session shutdown. LE GATT/ATT/SMP, audio profiles, RFCOMM, serial, WebSocket,
and gRPC transports are outside its scope.

The extraction baseline is `niart120/bumble-rs` revision
`cb55e2d98dc7b7b0227c43772c9ae184034dd9a1`. Derived source retains the
Apache-2.0 license and attribution in `LICENSE` and `NOTICE`.

The crate remains `publish = false` until its protocol, transport, archive,
and `swbt-rs` integration gates are complete. This repository does not grant
authorization to publish it.
