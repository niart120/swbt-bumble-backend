// Derived from chaitanyarahalkar/bumble-rs at bbac2a6 via the modified
// niart120/bumble-rs fork at cb55e2d. Rewritten to retain command, event, and
// ACL families only; SCO and ISO are intentionally absent. See PROVENANCE.md.

use std::collections::BTreeMap;
use std::fmt;

const COMMAND_PACKET_TYPE: u8 = 0x01;
const ACL_PACKET_TYPE: u8 = 0x02;
const EVENT_PACKET_TYPE: u8 = 0x04;
const ACL_HANDLE_MASK: u16 = 0x0FFF;
const ACL_BOUNDARY_MASK: u16 = 0x0003;
const ACL_BROADCAST_MASK: u16 = 0x0003;
const ACL_FIRST_NON_FLUSHABLE: u8 = 0;
const ACL_CONTINUATION: u8 = 1;
const ACL_FIRST_FLUSHABLE: u8 = 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CodecError {
    EmptyPacket,
    UnsupportedPacketType(u8),
    Truncated { field: &'static str },
    TrailingBytes { expected: usize, actual: usize },
    ParameterTooLong { actual: usize, maximum: usize },
    InvalidConnectionHandle(u16),
    InvalidPacketBoundary(u8),
    InvalidBroadcastFlag(u8),
    InvalidL2capPdu,
    UnexpectedContinuation { connection_handle: u16 },
    ContinuationOverflow { expected: usize, actual: usize },
}

impl fmt::Display for CodecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for CodecError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CommandPacket {
    pub(crate) opcode: u16,
    pub(crate) parameters: Vec<u8>,
}

impl CommandPacket {
    pub(crate) fn new(opcode: u16, parameters: Vec<u8>) -> Result<Self, CodecError> {
        validate_u8_length(parameters.len())?;
        Ok(Self { opcode, parameters })
    }

    fn from_bytes(packet: &[u8]) -> Result<Self, CodecError> {
        require_packet_type(packet, COMMAND_PACKET_TYPE)?;
        if packet.len() < 4 {
            return Err(CodecError::Truncated { field: "command" });
        }
        let parameter_length = usize::from(packet[3]);
        validate_exact_length(packet.len(), 4 + parameter_length)?;
        Self::new(
            u16::from_le_bytes([packet[1], packet[2]]),
            packet[4..].to_vec(),
        )
    }

    fn to_bytes(&self) -> Vec<u8> {
        let mut packet = Vec::with_capacity(4 + self.parameters.len());
        packet.push(COMMAND_PACKET_TYPE);
        packet.extend_from_slice(&self.opcode.to_le_bytes());
        packet.push(self.parameters.len() as u8);
        packet.extend_from_slice(&self.parameters);
        packet
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EventPacket {
    pub(crate) event_code: u8,
    pub(crate) parameters: Vec<u8>,
}

impl EventPacket {
    pub(crate) fn new(event_code: u8, parameters: Vec<u8>) -> Result<Self, CodecError> {
        validate_u8_length(parameters.len())?;
        Ok(Self {
            event_code,
            parameters,
        })
    }

    fn from_bytes(packet: &[u8]) -> Result<Self, CodecError> {
        require_packet_type(packet, EVENT_PACKET_TYPE)?;
        if packet.len() < 3 {
            return Err(CodecError::Truncated { field: "event" });
        }
        let parameter_length = usize::from(packet[2]);
        validate_exact_length(packet.len(), 3 + parameter_length)?;
        Self::new(packet[1], packet[3..].to_vec())
    }

    fn to_bytes(&self) -> Vec<u8> {
        let mut packet = Vec::with_capacity(3 + self.parameters.len());
        packet.extend_from_slice(&[
            EVENT_PACKET_TYPE,
            self.event_code,
            self.parameters.len() as u8,
        ]);
        packet.extend_from_slice(&self.parameters);
        packet
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AclPacket {
    pub(crate) connection_handle: u16,
    pub(crate) packet_boundary: u8,
    pub(crate) broadcast_flag: u8,
    pub(crate) data: Vec<u8>,
}

impl AclPacket {
    pub(crate) fn new(
        connection_handle: u16,
        packet_boundary: u8,
        broadcast_flag: u8,
        data: Vec<u8>,
    ) -> Result<Self, CodecError> {
        if connection_handle > ACL_HANDLE_MASK {
            return Err(CodecError::InvalidConnectionHandle(connection_handle));
        }
        if u16::from(packet_boundary) > ACL_BOUNDARY_MASK {
            return Err(CodecError::InvalidPacketBoundary(packet_boundary));
        }
        if u16::from(broadcast_flag) > ACL_BROADCAST_MASK {
            return Err(CodecError::InvalidBroadcastFlag(broadcast_flag));
        }
        if data.len() > usize::from(u16::MAX) {
            return Err(CodecError::ParameterTooLong {
                actual: data.len(),
                maximum: usize::from(u16::MAX),
            });
        }
        Ok(Self {
            connection_handle,
            packet_boundary,
            broadcast_flag,
            data,
        })
    }

    fn from_bytes(packet: &[u8]) -> Result<Self, CodecError> {
        require_packet_type(packet, ACL_PACKET_TYPE)?;
        if packet.len() < 5 {
            return Err(CodecError::Truncated {
                field: "ACL packet",
            });
        }
        let header = u16::from_le_bytes([packet[1], packet[2]]);
        let data_length = usize::from(u16::from_le_bytes([packet[3], packet[4]]));
        validate_exact_length(packet.len(), 5 + data_length)?;
        Self::new(
            header & ACL_HANDLE_MASK,
            ((header >> 12) & ACL_BOUNDARY_MASK) as u8,
            ((header >> 14) & ACL_BROADCAST_MASK) as u8,
            packet[5..].to_vec(),
        )
    }

    fn to_bytes(&self) -> Vec<u8> {
        let header = self.connection_handle
            | (u16::from(self.packet_boundary) << 12)
            | (u16::from(self.broadcast_flag) << 14);
        let mut packet = Vec::with_capacity(5 + self.data.len());
        packet.push(ACL_PACKET_TYPE);
        packet.extend_from_slice(&header.to_le_bytes());
        packet.extend_from_slice(&(self.data.len() as u16).to_le_bytes());
        packet.extend_from_slice(&self.data);
        packet
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Packet {
    Command(CommandPacket),
    Event(EventPacket),
    Acl(AclPacket),
}

impl Packet {
    pub(crate) fn from_bytes(packet: &[u8]) -> Result<Self, CodecError> {
        match packet.first().copied().ok_or(CodecError::EmptyPacket)? {
            COMMAND_PACKET_TYPE => CommandPacket::from_bytes(packet).map(Self::Command),
            EVENT_PACKET_TYPE => EventPacket::from_bytes(packet).map(Self::Event),
            ACL_PACKET_TYPE => AclPacket::from_bytes(packet).map(Self::Acl),
            packet_type => Err(CodecError::UnsupportedPacketType(packet_type)),
        }
    }

    pub(crate) fn to_bytes(&self) -> Vec<u8> {
        match self {
            Self::Command(packet) => packet.to_bytes(),
            Self::Event(packet) => packet.to_bytes(),
            Self::Acl(packet) => packet.to_bytes(),
        }
    }
}

pub(crate) fn fragment_l2cap_pdu(
    connection_handle: u16,
    broadcast_flag: u8,
    acl_payload_size: usize,
    pdu: &[u8],
    flushable: bool,
) -> Result<Vec<AclPacket>, CodecError> {
    if acl_payload_size == 0 || pdu.len() < 4 {
        return Err(CodecError::InvalidL2capPdu);
    }
    let payload_length = usize::from(u16::from_le_bytes([pdu[0], pdu[1]]));
    if payload_length + 4 != pdu.len() {
        return Err(CodecError::InvalidL2capPdu);
    }
    pdu.chunks(acl_payload_size)
        .enumerate()
        .map(|(index, data)| {
            AclPacket::new(
                connection_handle,
                if index == 0 {
                    if flushable {
                        ACL_FIRST_FLUSHABLE
                    } else {
                        ACL_FIRST_NON_FLUSHABLE
                    }
                } else {
                    ACL_CONTINUATION
                },
                broadcast_flag,
                data.to_vec(),
            )
        })
        .collect()
}

#[derive(Default)]
pub(crate) struct AclAssembler {
    pending: BTreeMap<u16, PendingAcl>,
}

struct PendingAcl {
    expected: usize,
    data: Vec<u8>,
}

impl AclAssembler {
    pub(crate) fn feed(&mut self, packet: &AclPacket) -> Result<Option<Vec<u8>>, CodecError> {
        if packet.packet_boundary == ACL_CONTINUATION {
            let pending = self.pending.get_mut(&packet.connection_handle).ok_or(
                CodecError::UnexpectedContinuation {
                    connection_handle: packet.connection_handle,
                },
            )?;
            pending.data.extend_from_slice(&packet.data);
            if pending.data.len() > pending.expected {
                let expected = pending.expected;
                let actual = pending.data.len();
                self.pending.remove(&packet.connection_handle);
                return Err(CodecError::ContinuationOverflow { expected, actual });
            }
            if pending.data.len() == pending.expected {
                return Ok(self
                    .pending
                    .remove(&packet.connection_handle)
                    .map(|pending| pending.data));
            }
            return Ok(None);
        }

        if packet.data.len() < 4 {
            return Err(CodecError::InvalidL2capPdu);
        }
        let expected = usize::from(u16::from_le_bytes([packet.data[0], packet.data[1]])) + 4;
        if packet.data.len() > expected {
            return Err(CodecError::ContinuationOverflow {
                expected,
                actual: packet.data.len(),
            });
        }
        if packet.data.len() == expected {
            self.pending.remove(&packet.connection_handle);
            return Ok(Some(packet.data.clone()));
        }
        self.pending.insert(
            packet.connection_handle,
            PendingAcl {
                expected,
                data: packet.data.clone(),
            },
        );
        Ok(None)
    }

    pub(crate) fn clear_handle(&mut self, connection_handle: u16) {
        self.pending.remove(&connection_handle);
    }
}

fn require_packet_type(packet: &[u8], expected: u8) -> Result<(), CodecError> {
    match packet.first().copied() {
        Some(actual) if actual == expected => Ok(()),
        Some(actual) => Err(CodecError::UnsupportedPacketType(actual)),
        None => Err(CodecError::EmptyPacket),
    }
}

fn validate_u8_length(actual: usize) -> Result<(), CodecError> {
    if actual <= usize::from(u8::MAX) {
        Ok(())
    } else {
        Err(CodecError::ParameterTooLong {
            actual,
            maximum: usize::from(u8::MAX),
        })
    }
}

fn validate_exact_length(actual: usize, expected: usize) -> Result<(), CodecError> {
    if actual < expected {
        Err(CodecError::Truncated { field: "payload" })
    } else if actual > expected {
        Err(CodecError::TrailingBytes { expected, actual })
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_and_event_frames_match_bumble_oracles() {
        let reset = Packet::Command(CommandPacket::new(0x0C03, Vec::new()).unwrap());
        assert_eq!(reset.to_bytes(), [0x01, 0x03, 0x0C, 0x00]);
        assert_eq!(Packet::from_bytes(&reset.to_bytes()).unwrap(), reset);

        let complete = [0x04, 0x0E, 0x04, 0x01, 0x03, 0x0C, 0x00];
        assert_eq!(Packet::from_bytes(&complete).unwrap().to_bytes(), complete);
    }

    #[test]
    fn acl_fragments_reassemble_one_l2cap_pdu() {
        let mut pdu = vec![23, 0, 0x40, 0];
        pdu.extend(0_u8..23);
        let packets = fragment_l2cap_pdu(0x0123, 0, 9, &pdu, false).unwrap();

        assert_eq!(packets.len(), 3);
        assert_eq!(packets[0].packet_boundary, ACL_FIRST_NON_FLUSHABLE);
        assert!(
            packets[1..]
                .iter()
                .all(|packet| packet.packet_boundary == ACL_CONTINUATION)
        );

        let mut assembler = AclAssembler::default();
        let mut complete = None;
        for packet in &packets {
            assert_eq!(
                Packet::from_bytes(&packet.to_bytes()).unwrap(),
                Packet::Acl(packet.clone())
            );
            complete = assembler.feed(packet).unwrap().or(complete);
        }
        assert_eq!(complete.as_deref(), Some(pdu.as_slice()));
    }

    #[test]
    fn codec_rejects_sco_and_iso_packet_families() {
        assert_eq!(
            Packet::from_bytes(&[0x03, 0, 0, 0]).unwrap_err(),
            CodecError::UnsupportedPacketType(0x03)
        );
        assert_eq!(
            Packet::from_bytes(&[0x05, 0, 0, 0]).unwrap_err(),
            CodecError::UnsupportedPacketType(0x05)
        );
    }

    #[test]
    fn codec_rejects_declared_length_mismatch() {
        assert_eq!(
            Packet::from_bytes(&[0x01, 0x03, 0x0C, 0x01]).unwrap_err(),
            CodecError::Truncated { field: "payload" }
        );
        assert_eq!(
            Packet::from_bytes(&[0x04, 0x0E, 0x00, 0xFF]).unwrap_err(),
            CodecError::TrailingBytes {
                expected: 3,
                actual: 4
            }
        );
    }

    #[test]
    fn assembler_rejects_continuation_without_start() {
        let packet = AclPacket::new(7, ACL_CONTINUATION, 0, vec![1, 2]).unwrap();

        assert_eq!(
            AclAssembler::default().feed(&packet).unwrap_err(),
            CodecError::UnexpectedContinuation {
                connection_handle: 7
            }
        );
    }
}
