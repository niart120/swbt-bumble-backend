use std::error::Error as StdError;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use crate::{BluetoothAddress, ClassicBond};

const EXTENDED_INQUIRY_RESPONSE_LEN: usize = 240;
const COMPLETE_LOCAL_NAME_DATA_TYPE: u8 = 0x09;
const MAX_COMPLETE_LOCAL_NAME_LEN: usize = EXTENDED_INQUIRY_RESPONSE_LEN - 2;

/// Opaque selector for a USB Bluetooth adapter.
///
/// The selector is parsed only when a session is opened. Its debug output is
/// redacted because selectors can contain USB serial numbers.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct AdapterSelector(Box<str>);

impl AdapterSelector {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for AdapterSelector {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("AdapterSelector")
            .field(&"<redacted>")
            .finish()
    }
}

impl From<String> for AdapterSelector {
    fn from(value: String) -> Self {
        Self(value.into_boxed_str())
    }
}

impl From<&str> for AdapterSelector {
    fn from(value: &str) -> Self {
        Self(value.into())
    }
}

/// Callback invoked when the backend receives input or reaches a terminal state.
#[derive(Clone)]
pub struct ActivityNotifier {
    callback: Arc<dyn Fn() + Send + Sync>,
}

impl ActivityNotifier {
    /// Creates a notifier from a thread-safe callback.
    pub fn new(callback: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            callback: Arc::new(callback),
        }
    }

    /// Invokes the configured activity callback.
    pub fn notify(&self) {
        (self.callback)();
    }
}

impl fmt::Debug for ActivityNotifier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ActivityNotifier")
            .finish_non_exhaustive()
    }
}

/// Bluetooth identity policy applied before controller initialization.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LocalIdentity {
    /// Retain the public address already reported by the adapter.
    #[default]
    AdapterDefault,
    /// Require the adapter to use the supplied public address.
    Explicit(BluetoothAddress),
}

/// SDP policy fields for one Bluetooth Classic HID service record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HidSdpPolicy {
    /// Primary service name advertised through SDP.
    pub service_name: Box<str>,
    /// Optional human-readable service description.
    pub service_description: Option<Box<str>>,
    /// Optional provider name.
    pub provider_name: Option<Box<str>>,
    /// Optional HID device release number.
    pub device_release_number: Option<u16>,
    /// Bluetooth profile descriptor version.
    pub bluetooth_profile_version: u16,
    /// HID parser version.
    pub parser_version: u16,
    /// HID device subclass.
    pub device_subclass: u8,
    /// HID country code.
    pub country_code: u8,
    /// Whether the device supports a virtual cable.
    pub virtual_cable: bool,
    /// Whether the device initiates reconnects.
    pub reconnect_initiate: bool,
    /// Optional remote-wake capability.
    pub remote_wake: Option<bool>,
    /// HID profile version.
    pub profile_version: u16,
    /// Link supervision timeout.
    pub supervision_timeout: u16,
    /// Whether the device is normally connectable.
    pub normally_connectable: bool,
    /// Whether the device supports the HID boot protocol.
    pub boot_device: bool,
    /// SSR host maximum latency.
    pub ssr_host_max_latency: u16,
    /// SSR host minimum timeout.
    pub ssr_host_min_timeout: u16,
}

/// HID report descriptor and SDP policy used by a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HidServiceConfig {
    report_descriptor: Box<[u8]>,
    sdp_policy: HidSdpPolicy,
}

impl HidServiceConfig {
    /// Creates a HID service definition.
    pub fn new(report_descriptor: impl Into<Box<[u8]>>, sdp_policy: HidSdpPolicy) -> Self {
        Self {
            report_descriptor: report_descriptor.into(),
            sdp_policy,
        }
    }

    /// Returns the HID report descriptor advertised through SDP.
    pub fn report_descriptor(&self) -> &[u8] {
        &self.report_descriptor
    }

    /// Returns the SDP policy.
    pub const fn sdp_policy(&self) -> &HidSdpPolicy {
        &self.sdp_policy
    }
}

/// Controller identity and HID service settings for one session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionConfig {
    local_name: Box<str>,
    class_of_device: u32,
    extended_inquiry_response: [u8; EXTENDED_INQUIRY_RESPONSE_LEN],
    complete_local_name_eir_len: usize,
    hid_service: HidServiceConfig,
}

impl SessionConfig {
    /// Builds Classic-only controller settings and a complete-local-name EIR.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidConfiguration`] when `local_name` is empty,
    /// does not fit the 240-byte EIR, or `class_of_device` exceeds 24 bits.
    pub fn new(
        local_name: impl Into<Box<str>>,
        class_of_device: u32,
        hid_service: HidServiceConfig,
    ) -> Result<Self, Error> {
        let local_name = local_name.into();
        let name_bytes = local_name.as_bytes();
        if name_bytes.is_empty()
            || name_bytes.len() > MAX_COMPLETE_LOCAL_NAME_LEN
            || class_of_device > 0x00ff_ffff
        {
            return Err(Error::new(ErrorKind::InvalidConfiguration));
        }

        let mut extended_inquiry_response = [0; EXTENDED_INQUIRY_RESPONSE_LEN];
        extended_inquiry_response[0] = (name_bytes.len() + 1) as u8;
        extended_inquiry_response[1] = COMPLETE_LOCAL_NAME_DATA_TYPE;
        extended_inquiry_response[2..2 + name_bytes.len()].copy_from_slice(name_bytes);
        let complete_local_name_eir_len = name_bytes.len() + 2;

        Ok(Self {
            local_name,
            class_of_device,
            extended_inquiry_response,
            complete_local_name_eir_len,
            hid_service,
        })
    }

    /// Returns the controller local name.
    pub fn local_name(&self) -> &str {
        &self.local_name
    }

    /// Returns the 24-bit Bluetooth Class of Device value.
    pub const fn class_of_device(&self) -> u32 {
        self.class_of_device
    }

    /// Returns the populated complete-local-name EIR field without zero padding.
    pub fn complete_local_name_eir(&self) -> &[u8] {
        &self.extended_inquiry_response[..self.complete_local_name_eir_len]
    }

    /// Returns the HID service definition.
    pub const fn hid_service(&self) -> &HidServiceConfig {
        &self.hid_service
    }

    pub(crate) const fn extended_inquiry_response(&self) -> &[u8; EXTENDED_INQUIRY_RESPONSE_LEN] {
        &self.extended_inquiry_response
    }
}

/// Inputs retained by a newly opened backend session.
#[derive(Clone, Debug)]
pub struct OpenOptions {
    adapter: AdapterSelector,
    config: SessionConfig,
    local_identity: LocalIdentity,
    activity: ActivityNotifier,
}

impl OpenOptions {
    /// Creates options that retain the adapter's current public address.
    pub fn new(
        adapter: AdapterSelector,
        config: SessionConfig,
        activity: ActivityNotifier,
    ) -> Self {
        Self {
            adapter,
            config,
            local_identity: LocalIdentity::AdapterDefault,
            activity,
        }
    }

    /// Selects the controller identity policy.
    #[must_use]
    pub fn with_local_identity(mut self, local_identity: LocalIdentity) -> Self {
        self.local_identity = local_identity;
        self
    }

    /// Returns the redacted adapter selector.
    pub const fn adapter(&self) -> &AdapterSelector {
        &self.adapter
    }

    /// Returns the controller and HID settings.
    pub const fn config(&self) -> &SessionConfig {
        &self.config
    }

    /// Returns the controller identity policy.
    pub const fn local_identity(&self) -> LocalIdentity {
        self.local_identity
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        AdapterSelector,
        SessionConfig,
        LocalIdentity,
        ActivityNotifier,
    ) {
        (
            self.adapter,
            self.config,
            self.local_identity,
            self.activity,
        )
    }
}

/// Failure returned by a caller-provided Classic bond store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum BondStoreError {
    /// One peer's bond could not be read.
    LoadFailed,
    /// The set of reconnectable bonds could not be listed.
    ListFailed,
    /// A new or replacement bond could not be persisted.
    UpsertFailed,
}

impl fmt::Display for BondStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::LoadFailed => "Classic bond could not be read",
            Self::ListFailed => "Classic bonds could not be listed",
            Self::UpsertFailed => "Classic bond could not be persisted",
        };
        formatter.write_str(message)
    }
}

impl StdError for BondStoreError {}

/// Storage boundary for Classic link keys used by pairing and reconnect.
pub trait BondStore: Send {
    /// Selects the namespace for the initialized local controller address.
    ///
    /// The session calls this once after HCI initialization and before any
    /// bond lookup. Stores without local-controller namespaces may keep the
    /// default no-op implementation.
    ///
    /// # Errors
    ///
    /// Returns [`BondStoreError`] when the selected namespace cannot be used.
    fn select_local_address(
        &mut self,
        _local_address: BluetoothAddress,
    ) -> Result<(), BondStoreError> {
        Ok(())
    }

    /// Loads the bond for one peer.
    ///
    /// # Errors
    ///
    /// Returns [`BondStoreError`] when the backing store cannot be read.
    fn load(&self, peer: BluetoothAddress) -> Result<Option<ClassicBond>, BondStoreError>;

    /// Lists all bonds in the current local-controller namespace.
    ///
    /// # Errors
    ///
    /// Returns [`BondStoreError`] when the backing store cannot be read.
    fn load_all(&self) -> Result<Vec<(BluetoothAddress, ClassicBond)>, BondStoreError>;

    /// Persists a new or replacement bond for one peer.
    ///
    /// # Errors
    ///
    /// Returns [`BondStoreError`] when the backing store cannot be updated.
    fn upsert(&mut self, peer: BluetoothAddress, bond: ClassicBond) -> Result<(), BondStoreError>;
}

impl<T: BondStore + ?Sized> BondStore for Box<T> {
    fn select_local_address(
        &mut self,
        local_address: BluetoothAddress,
    ) -> Result<(), BondStoreError> {
        (**self).select_local_address(local_address)
    }

    fn load(&self, peer: BluetoothAddress) -> Result<Option<ClassicBond>, BondStoreError> {
        (**self).load(peer)
    }

    fn load_all(&self) -> Result<Vec<(BluetoothAddress, ClassicBond)>, BondStoreError> {
        (**self).load_all()
    }

    fn upsert(&mut self, peer: BluetoothAddress, bond: ClassicBond) -> Result<(), BondStoreError> {
        (**self).upsert(peer, bond)
    }
}

/// Controller version reported during HCI initialization.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ControllerVersion {
    /// HCI specification version.
    pub hci_version: u8,
    /// Manufacturer-defined HCI revision.
    pub hci_subversion: u16,
    /// Link Manager specification version.
    pub lmp_version: u8,
    /// Bluetooth SIG company identifier.
    pub company_identifier: u16,
    /// Manufacturer-defined Link Manager subversion.
    pub lmp_subversion: u16,
}

/// USB adapter location and identity metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsbAdapterMetadata {
    /// USB vendor identifier.
    pub vendor_id: u16,
    /// USB product identifier.
    pub product_id: u16,
    /// USB bus number.
    pub bus: u8,
    /// Address assigned by the USB host.
    pub device_address: u8,
    /// Physical USB port path from the root hub.
    pub ports: Box<[u8]>,
}

/// Validated controller capabilities retained by an open session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Capabilities {
    local_address: BluetoothAddress,
    controller_version: ControllerVersion,
    usb: UsbAdapterMetadata,
}

impl Capabilities {
    pub(crate) const fn new(
        local_address: BluetoothAddress,
        controller_version: ControllerVersion,
        usb: UsbAdapterMetadata,
    ) -> Self {
        Self {
            local_address,
            controller_version,
            usb,
        }
    }

    /// Returns the initialized public controller address.
    pub const fn local_address(&self) -> BluetoothAddress {
        self.local_address
    }

    /// Returns the initialized controller version.
    pub const fn controller_version(&self) -> ControllerVersion {
        self.controller_version
    }

    /// Returns USB adapter metadata.
    pub const fn usb(&self) -> &UsbAdapterMetadata {
        &self.usb
    }
}

/// HID L2CAP channel visible to the application.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    /// HID control channel on PSM 0x0011.
    Control,
    /// HID interrupt channel on PSM 0x0013.
    Interrupt,
}

/// Application-visible output from a backend session.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Event {
    /// A Classic ACL connection was established.
    Connected { peer: BluetoothAddress },
    /// One HID channel completed L2CAP setup.
    ChannelOpened { channel: Channel },
    /// The peer sent a HID output report.
    HidOutput {
        channel: Channel,
        payload: Box<[u8]>,
    },
    /// The Classic ACL connection ended.
    Disconnected { reason: Option<u8> },
}

/// Stable error classification for session operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorKind {
    /// Controller or HID configuration is invalid.
    InvalidConfiguration,
    /// The adapter could not be opened or initialized.
    OpenFailed,
    /// The controller returned an unusable public address.
    InvalidControllerIdentity,
    /// The controller address differs from the requested explicit identity.
    IdentityMismatch,
    /// An explicit identity write started but its final state is uncertain.
    AdapterIdentityRecoveryRequired,
    /// Required Bluetooth Classic or ACL capabilities are absent.
    UnsupportedController,
    /// The caller-provided bond store failed.
    InvalidBondStore,
    /// No single reconnectable Classic bond exists.
    NoBond,
    /// The session is closed.
    Closed,
    /// The interrupt report could not enter the host-side queue.
    SendRejected,
    /// Pending host-side interrupt data was not drained before the deadline.
    DrainTimedOut,
    /// The bounded application event queue overflowed.
    EventQueueOverflow,
    /// The USB packet source ended or failed.
    SourceTerminated,
    /// The reader thread or USB handle could not be closed cleanly.
    CloseFailed,
    /// An HCI, L2CAP, SDP, or HIDP packet violated the required protocol.
    ProtocolViolation,
}

/// Error returned by a backend session operation.
#[derive(Clone)]
pub struct Error {
    kind: ErrorKind,
    source: Option<Arc<dyn StdError + Send + Sync>>,
}

impl Error {
    pub(crate) const fn new(kind: ErrorKind) -> Self {
        Self { kind, source: None }
    }

    pub(crate) fn with_source(
        kind: ErrorKind,
        source: impl StdError + Send + Sync + 'static,
    ) -> Self {
        Self {
            kind,
            source: Some(Arc::new(source)),
        }
    }

    /// Returns the stable error classification.
    pub const fn kind(&self) -> ErrorKind {
        self.kind
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Error")
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self.kind {
            ErrorKind::InvalidConfiguration => "backend configuration is invalid",
            ErrorKind::OpenFailed => "Bluetooth adapter could not be opened or initialized",
            ErrorKind::InvalidControllerIdentity => {
                "Bluetooth controller returned an invalid identity"
            }
            ErrorKind::IdentityMismatch => {
                "Bluetooth controller identity does not match the requested identity"
            }
            ErrorKind::AdapterIdentityRecoveryRequired => {
                "Bluetooth adapter identity is uncertain after a write"
            }
            ErrorKind::UnsupportedController => {
                "Bluetooth controller lacks required Classic ACL capability"
            }
            ErrorKind::InvalidBondStore => "Classic bond store could not be accessed",
            ErrorKind::NoBond => "no single reconnectable Classic bond exists",
            ErrorKind::Closed => "backend session is closed",
            ErrorKind::SendRejected => "backend session rejected the interrupt report",
            ErrorKind::DrainTimedOut => "backend interrupt queue did not drain before the deadline",
            ErrorKind::EventQueueOverflow => "backend application event queue overflowed",
            ErrorKind::SourceTerminated => "Bluetooth packet source terminated",
            ErrorKind::CloseFailed => "backend session could not be closed cleanly",
            ErrorKind::ProtocolViolation => "Bluetooth protocol packet is invalid or unsupported",
        };
        formatter.write_str(message)
    }
}

impl StdError for Error {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.source
            .as_ref()
            .map(|source| source.as_ref() as &(dyn StdError + 'static))
    }
}

pub(crate) trait SessionDriver: Send {
    fn capabilities(&self) -> &Capabilities;
    fn start_pairing(&mut self) -> Result<(), Error>;
    fn start_reconnect(&mut self) -> Result<(), Error>;
    fn poll(&mut self, timeout: Duration) -> Result<Vec<Event>, Error>;
    fn interrupt_send_capacity_available(&self) -> bool;
    fn send_interrupt(&mut self, payload: &[u8]) -> Result<(), Error>;
    fn drain_interrupt(&mut self, timeout: Duration) -> Result<(), Error>;
    fn disconnect(&mut self) -> Result<(), Error>;
    fn close(&mut self) -> Result<(), Error>;
}

/// Opaque, owned Bluetooth Classic HID session.
///
/// The session owns controller initialization, the USB packet reader, and the
/// Classic protocol state without exposing HCI or L2CAP types.
pub struct Session {
    pub(crate) driver: Box<dyn SessionDriver>,
}

impl Session {
    /// Returns validated controller and adapter capabilities.
    pub fn capabilities(&self) -> &Capabilities {
        self.driver.capabilities()
    }

    /// Enables incoming pairing with the first Classic peer.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] when the session is closed or controller commands fail.
    pub fn start_pairing(&mut self) -> Result<(), Error> {
        self.driver.start_pairing()
    }

    /// Reconnects the only Classic peer retained by the bond store.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::NoBond`] when there is not exactly one usable bond.
    pub fn start_reconnect(&mut self) -> Result<(), Error> {
        self.driver.start_reconnect()
    }

    /// Polls application-visible events until one is available or `timeout` expires.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] after a terminal source, protocol, or storage failure.
    pub fn poll(&mut self, timeout: Duration) -> Result<Vec<Event>, Error> {
        self.driver.poll(timeout)
    }

    /// Reports whether an interrupt input report can be sent without waiting
    /// in the host-side ACL queue.
    pub fn interrupt_send_capacity_available(&self) -> bool {
        self.driver.interrupt_send_capacity_available()
    }

    /// Queues one application payload on the HID interrupt channel.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::SendRejected`] when the interrupt channel is not
    /// open or the payload is invalid for the negotiated channel.
    pub fn send_interrupt(&mut self, payload: &[u8]) -> Result<(), Error> {
        self.driver.send_interrupt(payload)
    }

    /// Waits for host-side interrupt data to enter the controller flow window.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::DrainTimedOut`] when the deadline expires.
    pub fn drain_interrupt(&mut self, timeout: Duration) -> Result<(), Error> {
        self.driver.drain_interrupt(timeout)
    }

    /// Requests an orderly Classic ACL disconnect.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] when the disconnect command cannot be queued.
    pub fn disconnect(&mut self) -> Result<(), Error> {
        self.driver.disconnect()
    }

    /// Cancels and joins the packet reader, then releases the USB handle.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::CloseFailed`] when shutdown cannot complete cleanly.
    pub fn close(&mut self) -> Result<(), Error> {
        self.driver.close()
    }
}

impl fmt::Debug for Session {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Session").finish_non_exhaustive()
    }
}
