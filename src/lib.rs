//! Bluetooth Classic HID backend extracted for `swbt-rs`.
//!
//! This package is not yet published. The first implementation slices keep
//! all Bumble-derived protocol and transport types private; the stable
//! session API will be added only after those slices have behavioral tests.

#![forbid(unsafe_code)]

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the internal codec is consumed by the T06c-T06e extraction slices"
    )
)]
mod hci;
#[expect(
    dead_code,
    reason = "the internal Classic protocol is consumed by the T06d host slice"
)]
mod hidp;
#[expect(
    dead_code,
    reason = "the internal Classic protocol is consumed by the T06d host slice"
)]
mod l2cap;
#[expect(
    dead_code,
    reason = "the internal Classic protocol is consumed by the T06d host slice"
)]
mod sdp;
mod values;

pub use values::{AddressKind, BluetoothAddress, BluetoothUuid, ClassicBond, ValueError};

/// Fixed Bumble fork revision used as the extraction baseline.
pub const EXTRACTION_SOURCE_REVISION: &str = "cb55e2d98dc7b7b0227c43772c9ae184034dd9a1";

#[cfg(test)]
mod tests {
    use super::EXTRACTION_SOURCE_REVISION;

    #[test]
    fn extraction_source_revision_is_a_full_git_object_id() {
        assert_eq!(EXTRACTION_SOURCE_REVISION.len(), 40);
        assert!(
            EXTRACTION_SOURCE_REVISION
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        );
    }
}
