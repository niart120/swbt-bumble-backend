# swbt-bumble-backend

`swbt-bumble-backend` is the Bluetooth Classic HID backend extracted for
[`swbt-rs`](https://github.com/niart120/swbt-rs).

The package boundary is intentionally narrower than the Bumble workspace. It
contains USB HCI, the required HCI command/event/ACL codecs, Bluetooth
Classic pairing and reconnect state, Classic L2CAP, SDP, HIDP, and ordered
session shutdown. LE GATT/ATT/SMP, audio profiles, RFCOMM, serial, WebSocket,
and gRPC transports are outside its scope.

The extraction baseline is `niart120/bumble-rs` revision
`cb55e2d98dc7b7b0227c43772c9ae184034dd9a1`. Derived source retains the
Apache-2.0 license and attribution in `LICENSE` and `NOTICE`.

Version 0.1.0 is prepared as the first crates.io release. Uploading a release
requires explicit authorization separate from this repository state.
