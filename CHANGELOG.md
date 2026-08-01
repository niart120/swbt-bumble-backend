# Changelog

This file records user-visible changes to `swbt-bumble-backend`.

## 0.1.0 - 2026-08-01

- Extract the Bluetooth Classic HID subset required by `swbt-rs` behind an
  owned `Session` API.
- Support USB HCI initialization, pairing, stored-bond reconnect, Classic
  L2CAP, SDP, HIDP reports, flow control, and ordered shutdown.
- Preserve the Apache-2.0 license and attribution of the Bumble-derived
  implementation.
- Record the exact upstream and intermediate-fork revisions, source mapping,
  and file-level modification notices.
