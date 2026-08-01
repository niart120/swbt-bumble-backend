//! Bluetooth Classic HID backend extracted for `swbt-rs`.
//!
//! This package is not yet published. Bumble-derived protocol and transport
//! types remain private behind the owned [`Session`] API.

#![forbid(unsafe_code)]

mod api;
#[expect(
    dead_code,
    reason = "the private host retains tested helpers used by extracted protocol fixtures"
)]
mod classic_host;
mod csr;
mod hci;
mod hid_service;
#[expect(
    dead_code,
    reason = "the private HIDP codec retains tested message forms outside the session subset"
)]
mod hidp;
mod identity;
#[expect(
    dead_code,
    reason = "the private L2CAP codec retains tested ERTM paths outside the session subset"
)]
mod l2cap;
#[expect(
    dead_code,
    reason = "the private SDP codec retains tested client paths outside the session subset"
)]
mod sdp;
mod session;
#[expect(
    dead_code,
    reason = "the private USB transport retains scripted test helpers outside the session subset"
)]
mod usb;
mod values;

pub use api::{
    ActivityNotifier, AdapterSelector, BondStore, BondStoreError, Capabilities, Channel,
    ControllerVersion, Error, ErrorKind, Event, HidSdpPolicy, HidServiceConfig, LocalIdentity,
    OpenOptions, Session, SessionConfig, UsbAdapterMetadata,
};
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
