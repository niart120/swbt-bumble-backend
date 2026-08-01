# Changelog

This file records user-visible changes to `swbt-bumble-backend`.

## 0.1.1 - 2026-08-02

- Use the legacy LE event mask for HCI version 6 and earlier so older
  adapters such as CSR8510 A10 complete initialization.
- Queue explicit HID interrupt reports behind in-flight ACL credit so tap
  release and trailing neutral reports complete under backpressure.
- Verify fresh pairing, stored-bond Periodic and Direct reconnect, input, IMU,
  neutral close, power-cycle recovery, and adapter reuse on Windows 11 with a
  CSR8510 A10 and Switch 2 system version 22.5.0.

## 0.1.0 - 2026-08-01

- Extract the Bluetooth Classic HID subset required by `swbt-rs` behind an
  owned `Session` API.
- Support USB HCI initialization, pairing, stored-bond reconnect, Classic
  L2CAP, SDP, HIDP reports, flow control, and ordered shutdown.
- Preserve the Apache-2.0 license and attribution of the Bumble-derived
  implementation.
- Record the exact upstream and intermediate-fork revisions, source mapping,
  and file-level modification notices.
