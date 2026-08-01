// Derived from chaitanyarahalkar/bumble-rs at bbac2a6 via the modified
// niart120/bumble-rs fork at cb55e2d. Rewritten for the public backend
// boundary and reduced to Classic HID needs. See PROVENANCE.md.

use std::fmt;
use std::hash::{Hash, Hasher};

const BASE_UUID_LE: [u8; 12] = [
    0xFB, 0x34, 0x9B, 0x5F, 0x80, 0x00, 0x00, 0x80, 0x00, 0x10, 0x00, 0x00,
];

/// Invalid backend value input.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ValueError {
    /// A Bluetooth address has the wrong width or contains a non-hexadecimal octet.
    InvalidAddress(String),
    /// A Bluetooth UUID has a width other than 16, 32, or 128 bits.
    InvalidUuid(String),
    /// A Classic link key does not contain the controller-defined 16 bytes.
    InvalidLinkKeyLength { actual: usize },
}

impl fmt::Display for ValueError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidAddress(message) => {
                write!(formatter, "invalid Bluetooth address: {message}")
            }
            Self::InvalidUuid(message) => write!(formatter, "invalid Bluetooth UUID: {message}"),
            Self::InvalidLinkKeyLength { actual } => {
                write!(formatter, "Classic link key must be 16 bytes, got {actual}")
            }
        }
    }
}

impl std::error::Error for ValueError {}

/// Address qualifier used at the backend boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AddressKind {
    Public,
    Random,
}

/// Six-byte Bluetooth device address.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BluetoothAddress {
    bytes_le: [u8; 6],
    kind: AddressKind,
}

impl BluetoothAddress {
    /// Creates an address from little-endian controller bytes.
    pub const fn from_le_bytes(bytes_le: [u8; 6], kind: AddressKind) -> Self {
        Self { bytes_le, kind }
    }

    /// Parses a big-endian address with optional colon separators.
    ///
    /// # Errors
    ///
    /// Returns [`ValueError::InvalidAddress`] when the input does not contain
    /// six hexadecimal octets. A `/P` suffix selects [`AddressKind::Public`].
    pub fn parse(value: &str, kind: AddressKind) -> Result<Self, ValueError> {
        let (value, kind) = match value.strip_suffix("/P") {
            Some(value) => (value, AddressKind::Public),
            None => (value, kind),
        };
        let compact = match value.len() {
            12 if !value.contains(':') => value.to_owned(),
            17 => {
                let octets: Vec<_> = value.split(':').collect();
                if octets.len() != 6 || octets.iter().any(|octet| octet.len() != 2) {
                    return Err(ValueError::InvalidAddress(
                        "expected six two-digit octets".into(),
                    ));
                }
                octets.concat()
            }
            _ => {
                return Err(ValueError::InvalidAddress(
                    "expected 12 hexadecimal digits".into(),
                ));
            }
        };
        let mut bytes_le = [0_u8; 6];
        for (index, byte) in compact.as_bytes().chunks_exact(2).enumerate() {
            let pair = std::str::from_utf8(byte)
                .map_err(|_| ValueError::InvalidAddress("address is not UTF-8".into()))?;
            let parsed = u8::from_str_radix(pair, 16)
                .map_err(|_| ValueError::InvalidAddress(format!("invalid octet {pair:?}")))?;
            bytes_le[5 - index] = parsed;
        }
        Ok(Self { bytes_le, kind })
    }

    /// Returns the little-endian bytes used by HCI packets.
    pub const fn as_le_bytes(&self) -> &[u8; 6] {
        &self.bytes_le
    }

    /// Returns the address qualifier.
    pub const fn kind(&self) -> AddressKind {
        self.kind
    }
}

impl fmt::Display for BluetoothAddress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, byte) in self.bytes_le.iter().rev().enumerate() {
            if index > 0 {
                formatter.write_str(":")?;
            }
            write!(formatter, "{byte:02X}")?;
        }
        if self.kind == AddressKind::Public {
            formatter.write_str("/P")?;
        }
        Ok(())
    }
}

/// Bluetooth UUID stored in little-endian wire order.
#[derive(Clone, Debug)]
pub struct BluetoothUuid {
    bytes_le: Vec<u8>,
}

impl BluetoothUuid {
    /// Creates a 16-bit Bluetooth UUID.
    pub fn from_u16(value: u16) -> Self {
        Self {
            bytes_le: value.to_le_bytes().to_vec(),
        }
    }

    pub(crate) fn from_16_bits(value: u16) -> Self {
        Self::from_u16(value)
    }

    /// Creates a UUID from its 2-, 4-, or 16-byte little-endian form.
    ///
    /// # Errors
    ///
    /// Returns [`ValueError::InvalidUuid`] for every other byte length.
    pub fn from_le_bytes(bytes: &[u8]) -> Result<Self, ValueError> {
        match bytes.len() {
            2 | 4 | 16 => Ok(Self {
                bytes_le: bytes.to_vec(),
            }),
            length => Err(ValueError::InvalidUuid(format!(
                "expected 2, 4, or 16 bytes, got {length}"
            ))),
        }
    }

    pub(crate) fn from_bytes(bytes: &[u8]) -> Result<Self, ValueError> {
        Self::from_le_bytes(bytes)
    }

    /// Returns the UUID in its original little-endian width.
    pub fn as_le_bytes(&self) -> &[u8] {
        &self.bytes_le
    }

    pub(crate) fn to_bytes(&self, force_128: bool) -> Vec<u8> {
        if force_128 {
            self.expanded_le_bytes().to_vec()
        } else {
            self.bytes_le.clone()
        }
    }

    /// Returns the 128-bit Bluetooth-base expansion in little-endian order.
    pub fn expanded_le_bytes(&self) -> [u8; 16] {
        let mut expanded = [0_u8; 16];
        match self.bytes_le.len() {
            2 => {
                expanded[..12].copy_from_slice(&BASE_UUID_LE);
                expanded[12..14].copy_from_slice(&self.bytes_le);
            }
            4 => {
                expanded[..12].copy_from_slice(&BASE_UUID_LE);
                expanded[12..].copy_from_slice(&self.bytes_le);
            }
            16 => expanded.copy_from_slice(&self.bytes_le),
            _ => unreachable!("BluetoothUuid constructor validates its width"),
        }
        expanded
    }
}

impl PartialEq for BluetoothUuid {
    fn eq(&self, other: &Self) -> bool {
        self.expanded_le_bytes() == other.expanded_le_bytes()
    }
}

impl Eq for BluetoothUuid {}

impl Hash for BluetoothUuid {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.expanded_le_bytes().hash(state);
    }
}

/// Classic pairing material retained for reconnect.
#[derive(Clone, PartialEq, Eq)]
pub struct ClassicBond {
    link_key: [u8; 16],
    link_key_type: u8,
    authenticated: bool,
}

impl fmt::Debug for ClassicBond {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClassicBond")
            .field("link_key", &"<redacted>")
            .field("link_key_type", &self.link_key_type)
            .field("authenticated", &self.authenticated)
            .finish()
    }
}

impl ClassicBond {
    /// Creates a bond from a controller link key.
    pub const fn new(link_key: [u8; 16], link_key_type: u8, authenticated: bool) -> Self {
        Self {
            link_key,
            link_key_type,
            authenticated,
        }
    }

    /// Converts a dynamic key buffer into a fixed Classic bond.
    ///
    /// # Errors
    ///
    /// Returns [`ValueError::InvalidLinkKeyLength`] unless `link_key` contains
    /// exactly 16 bytes.
    pub fn from_slice(
        link_key: &[u8],
        link_key_type: u8,
        authenticated: bool,
    ) -> Result<Self, ValueError> {
        let actual = link_key.len();
        let link_key = link_key
            .try_into()
            .map_err(|_| ValueError::InvalidLinkKeyLength { actual })?;
        Ok(Self::new(link_key, link_key_type, authenticated))
    }

    /// Returns the 16-byte Classic link key.
    pub const fn link_key(&self) -> &[u8; 16] {
        &self.link_key
    }

    /// Returns the HCI link-key type.
    pub const fn link_key_type(&self) -> u8 {
        self.link_key_type
    }

    /// Returns whether the pairing authenticated the peer.
    pub const fn authenticated(&self) -> bool {
        self.authenticated
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_round_trips_controller_byte_order() {
        let address = BluetoothAddress::parse("00:11:22:33:44:55/P", AddressKind::Random).unwrap();

        assert_eq!(address.kind(), AddressKind::Public);
        assert_eq!(address.as_le_bytes(), &[0x55, 0x44, 0x33, 0x22, 0x11, 0x00]);
        assert_eq!(address.to_string(), "00:11:22:33:44:55/P");
    }

    #[test]
    fn address_rejects_irregular_separators() {
        let error = BluetoothAddress::parse("0:01122334455", AddressKind::Random).unwrap_err();

        assert!(matches!(error, ValueError::InvalidAddress(_)));
    }

    #[test]
    fn short_and_expanded_uuid_compare_equal() {
        let short = BluetoothUuid::from_u16(0x1234);
        let expanded = BluetoothUuid::from_le_bytes(&short.expanded_le_bytes()).unwrap();

        assert_eq!(short, expanded);
    }

    #[test]
    fn classic_bond_rejects_non_hci_key_length() {
        let error = ClassicBond::from_slice(&[0xAA; 15], 4, true).unwrap_err();

        assert_eq!(error, ValueError::InvalidLinkKeyLength { actual: 15 });
    }
}
