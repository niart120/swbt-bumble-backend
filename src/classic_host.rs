// Derived from bumble-host Classic state handling at cb55e2d. Rewritten for
// the swbt-bumble-backend Classic HID boundary without LE/GATT/SMP state.

use std::collections::{BTreeMap, VecDeque};
use std::fmt;

use crate::hci::{AclAssembler, AclPacket, CodecError, CommandPacket, fragment_l2cap_pdu};
use crate::l2cap::classic::{ChannelManager, ClassicChannelSpec};
use crate::l2cap::{self, L2capPdu};
use crate::{BluetoothAddress, ClassicBond};

const HCI_ACCEPT_CONNECTION_REQUEST: u16 = 0x0409;
const HCI_LINK_KEY_REQUEST_REPLY: u16 = 0x040B;
const HCI_LINK_KEY_REQUEST_NEGATIVE_REPLY: u16 = 0x040C;
const HCI_AUTHENTICATION_REQUESTED: u16 = 0x0411;
const HCI_SET_CONNECTION_ENCRYPTION: u16 = 0x0413;
const HCI_IO_CAPABILITY_REQUEST_REPLY: u16 = 0x042B;
const HCI_USER_CONFIRMATION_REQUEST_REPLY: u16 = 0x042C;
const HCI_ROLE_PERIPHERAL: u8 = 0x01;
const HCI_IO_CAPABILITY_NO_INPUT_NO_OUTPUT: u8 = 0x03;
const HCI_AUTHENTICATION_REQUIREMENTS_GENERAL_BONDING: u8 = 0x01;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BondStoreError {
    Load,
    Upsert,
    Delete,
}

pub(crate) trait BondStore {
    fn load(&self, peer: BluetoothAddress) -> Result<Option<ClassicBond>, BondStoreError>;
    fn upsert(&mut self, peer: BluetoothAddress, bond: ClassicBond) -> Result<(), BondStoreError>;
    fn delete(&mut self, peer: BluetoothAddress) -> Result<(), BondStoreError>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ClassicEvent {
    ConnectionRequest {
        peer: BluetoothAddress,
    },
    ConnectionComplete {
        status: u8,
        connection_handle: u16,
        peer: BluetoothAddress,
    },
    LinkKeyRequest {
        peer: BluetoothAddress,
    },
    LinkKeyNotification {
        peer: BluetoothAddress,
        link_key: [u8; 16],
        link_key_type: u8,
    },
    IoCapabilityRequest {
        peer: BluetoothAddress,
    },
    UserConfirmationRequest {
        peer: BluetoothAddress,
    },
    AuthenticationComplete {
        status: u8,
        connection_handle: u16,
    },
    EncryptionChange {
        status: u8,
        connection_handle: u16,
        enabled: bool,
    },
    NumberOfCompletedPackets {
        connection_handle: u16,
        completed: u16,
    },
    DisconnectionComplete {
        connection_handle: u16,
        reason: u8,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SessionEvent {
    Connected {
        peer: BluetoothAddress,
        connection_handle: u16,
    },
    BondStored {
        peer: BluetoothAddress,
    },
    Encrypted {
        peer: BluetoothAddress,
    },
    ChannelOpened {
        psm: u32,
        source_cid: u16,
    },
    Disconnected {
        peer: BluetoothAddress,
        reason: u8,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum HostOutput {
    Command(CommandPacket),
    Acl(AclPacket),
    Event(SessionEvent),
}

#[derive(Debug)]
pub(crate) enum HostError {
    BondStore(BondStoreError),
    Codec(CodecError),
    L2cap(l2cap::Error),
    NoConnection,
    ConnectionHandleMismatch {
        expected: u16,
        actual: u16,
    },
    PeerMismatch {
        expected: BluetoothAddress,
        actual: BluetoothAddress,
    },
    AclCreditUnderflow {
        in_flight: u16,
        completed: u16,
    },
}

impl fmt::Display for HostError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for HostError {}

impl From<CodecError> for HostError {
    fn from(error: CodecError) -> Self {
        Self::Codec(error)
    }
}

impl From<l2cap::Error> for HostError {
    fn from(error: l2cap::Error) -> Self {
        Self::L2cap(error)
    }
}

impl From<BondStoreError> for HostError {
    fn from(error: BondStoreError) -> Self {
        Self::BondStore(error)
    }
}

struct Connection {
    peer: BluetoothAddress,
    handle: u16,
    encrypted: bool,
    channels: ChannelManager,
}

struct AclFlow {
    payload_size: usize,
    total_packets: u16,
    in_flight: BTreeMap<u16, u16>,
    queued: VecDeque<AclPacket>,
}

impl AclFlow {
    fn new(payload_size: usize, total_packets: u16) -> Self {
        Self {
            payload_size,
            total_packets,
            in_flight: BTreeMap::new(),
            queued: VecDeque::new(),
        }
    }

    fn in_flight_total(&self) -> u16 {
        self.in_flight.values().copied().sum()
    }

    fn enqueue(&mut self, packets: impl IntoIterator<Item = AclPacket>) {
        self.queued.extend(packets);
    }

    fn flush(&mut self, output: &mut VecDeque<HostOutput>) {
        while self.in_flight_total() < self.total_packets {
            let Some(packet) = self.queued.pop_front() else {
                break;
            };
            *self.in_flight.entry(packet.connection_handle).or_default() += 1;
            output.push_back(HostOutput::Acl(packet));
        }
    }

    fn complete(&mut self, connection_handle: u16, completed: u16) -> Result<(), HostError> {
        let in_flight = self
            .in_flight
            .get(&connection_handle)
            .copied()
            .unwrap_or_default();
        if completed > in_flight {
            return Err(HostError::AclCreditUnderflow {
                in_flight,
                completed,
            });
        }
        let remaining = in_flight - completed;
        if remaining == 0 {
            self.in_flight.remove(&connection_handle);
        } else {
            self.in_flight.insert(connection_handle, remaining);
        }
        Ok(())
    }

    fn clear_handle(&mut self, connection_handle: u16) {
        self.in_flight.remove(&connection_handle);
        self.queued
            .retain(|packet| packet.connection_handle != connection_handle);
    }
}

pub(crate) struct ClassicHost<S> {
    bonds: S,
    connection: Option<Connection>,
    pending_peer: Option<BluetoothAddress>,
    servers: BTreeMap<u32, ClassicChannelSpec>,
    acl_assembler: AclAssembler,
    acl_flow: AclFlow,
    output: VecDeque<HostOutput>,
}

impl<S: BondStore> ClassicHost<S> {
    pub(crate) fn new(bonds: S, acl_payload_size: usize, total_acl_packets: u16) -> Self {
        Self {
            bonds,
            connection: None,
            pending_peer: None,
            servers: BTreeMap::new(),
            acl_assembler: AclAssembler::default(),
            acl_flow: AclFlow::new(acl_payload_size, total_acl_packets),
            output: VecDeque::new(),
        }
    }

    pub(crate) fn register_server(
        &mut self,
        psm: u32,
        spec: ClassicChannelSpec,
    ) -> Result<(), HostError> {
        if let Some(connection) = self.connection.as_mut() {
            connection.channels.register_server(Some(psm), spec)?;
        }
        self.servers.insert(psm, spec);
        Ok(())
    }

    pub(crate) fn handle_event(&mut self, event: ClassicEvent) -> Result<(), HostError> {
        match event {
            ClassicEvent::ConnectionRequest { peer } => {
                self.pending_peer = Some(peer);
                let mut parameters = peer.as_le_bytes().to_vec();
                parameters.push(HCI_ROLE_PERIPHERAL);
                self.queue_command(HCI_ACCEPT_CONNECTION_REQUEST, parameters)?;
            }
            ClassicEvent::ConnectionComplete {
                status: 0,
                connection_handle,
                peer,
            } => {
                if let Some(expected) = self.pending_peer {
                    if peer != expected {
                        return Err(HostError::PeerMismatch {
                            expected,
                            actual: peer,
                        });
                    }
                }
                let mut channels = ChannelManager::new();
                for (psm, spec) in &self.servers {
                    channels.register_server(Some(*psm), *spec)?;
                }
                self.pending_peer = None;
                self.connection = Some(Connection {
                    peer,
                    handle: connection_handle,
                    encrypted: false,
                    channels,
                });
                self.output
                    .push_back(HostOutput::Event(SessionEvent::Connected {
                        peer,
                        connection_handle,
                    }));
                self.queue_command(
                    HCI_AUTHENTICATION_REQUESTED,
                    connection_handle.to_le_bytes().to_vec(),
                )?;
            }
            ClassicEvent::ConnectionComplete { .. } => {
                self.pending_peer = None;
            }
            ClassicEvent::LinkKeyRequest { peer } => {
                let (opcode, mut parameters) = match self.bonds.load(peer)? {
                    Some(bond) => {
                        let mut parameters = peer.as_le_bytes().to_vec();
                        parameters.extend_from_slice(bond.link_key());
                        (HCI_LINK_KEY_REQUEST_REPLY, parameters)
                    }
                    None => (
                        HCI_LINK_KEY_REQUEST_NEGATIVE_REPLY,
                        peer.as_le_bytes().to_vec(),
                    ),
                };
                self.queue_command(opcode, std::mem::take(&mut parameters))?;
            }
            ClassicEvent::LinkKeyNotification {
                peer,
                link_key,
                link_key_type,
            } => {
                let authenticated = matches!(link_key_type, 0x05 | 0x08);
                self.bonds.upsert(
                    peer,
                    ClassicBond::new(link_key, link_key_type, authenticated),
                )?;
                self.output
                    .push_back(HostOutput::Event(SessionEvent::BondStored { peer }));
            }
            ClassicEvent::IoCapabilityRequest { peer } => {
                let mut parameters = peer.as_le_bytes().to_vec();
                parameters.extend_from_slice(&[
                    HCI_IO_CAPABILITY_NO_INPUT_NO_OUTPUT,
                    0,
                    HCI_AUTHENTICATION_REQUIREMENTS_GENERAL_BONDING,
                ]);
                self.queue_command(HCI_IO_CAPABILITY_REQUEST_REPLY, parameters)?;
            }
            ClassicEvent::UserConfirmationRequest { peer } => {
                self.queue_command(
                    HCI_USER_CONFIRMATION_REQUEST_REPLY,
                    peer.as_le_bytes().to_vec(),
                )?;
            }
            ClassicEvent::AuthenticationComplete {
                status: 0,
                connection_handle,
            } => {
                self.ensure_handle(connection_handle)?;
                let mut parameters = connection_handle.to_le_bytes().to_vec();
                parameters.push(1);
                self.queue_command(HCI_SET_CONNECTION_ENCRYPTION, parameters)?;
            }
            ClassicEvent::AuthenticationComplete { .. } => {}
            ClassicEvent::EncryptionChange {
                status: 0,
                connection_handle,
                enabled: true,
            } => {
                let connection = self.connection_mut(connection_handle)?;
                connection.encrypted = true;
                let peer = connection.peer;
                self.output
                    .push_back(HostOutput::Event(SessionEvent::Encrypted { peer }));
            }
            ClassicEvent::EncryptionChange { .. } => {}
            ClassicEvent::NumberOfCompletedPackets {
                connection_handle,
                completed,
            } => {
                self.acl_flow.complete(connection_handle, completed)?;
                self.acl_flow.flush(&mut self.output);
            }
            ClassicEvent::DisconnectionComplete {
                connection_handle,
                reason,
            } => {
                let connection = self.connection_mut(connection_handle)?;
                let peer = connection.peer;
                self.acl_flow.clear_handle(connection_handle);
                self.acl_assembler.clear_handle(connection_handle);
                self.output.retain(|output| {
                    !matches!(
                        output,
                        HostOutput::Acl(packet) if packet.connection_handle == connection_handle
                    )
                });
                self.connection = None;
                self.output
                    .push_back(HostOutput::Event(SessionEvent::Disconnected {
                        peer,
                        reason,
                    }));
            }
        }
        Ok(())
    }

    pub(crate) fn process_acl(&mut self, packet: AclPacket) -> Result<(), HostError> {
        let handle = self.connection_handle()?;
        if packet.connection_handle != handle {
            return Err(HostError::ConnectionHandleMismatch {
                expected: handle,
                actual: packet.connection_handle,
            });
        }
        let Some(bytes) = self.acl_assembler.feed(&packet)? else {
            return Ok(());
        };
        let pdu = L2capPdu::from_bytes(&bytes)?;
        self.connection
            .as_mut()
            .expect("connection handle was checked")
            .channels
            .process_pdu(pdu)?;
        self.flush_channels()
    }

    pub(crate) fn send_channel_sdu(
        &mut self,
        source_cid: u16,
        sdu: &[u8],
    ) -> Result<(), HostError> {
        self.connection
            .as_mut()
            .ok_or(HostError::NoConnection)?
            .channels
            .send(source_cid, sdu)?;
        self.flush_channels()
    }

    pub(crate) fn take_channel_sdu(&mut self, source_cid: u16) -> Option<Vec<u8>> {
        self.connection
            .as_mut()?
            .channels
            .channel_mut(source_cid)?
            .pop_received()
    }

    pub(crate) fn pop_output(&mut self) -> Option<HostOutput> {
        self.output.pop_front()
    }

    pub(crate) fn bond_store(&self) -> &S {
        &self.bonds
    }

    fn flush_channels(&mut self) -> Result<(), HostError> {
        let handle = self.connection_handle()?;
        let mut pdus = Vec::new();
        let mut opened = Vec::new();
        {
            let channels = &mut self
                .connection
                .as_mut()
                .expect("connection handle was checked")
                .channels;
            while let Some(pdu) = channels.poll_outbound() {
                pdus.push(pdu);
            }
            while let Some(source_cid) = channels.poll_accepted_channel() {
                let psm = channels
                    .channel(source_cid)
                    .expect("accepted channel remains registered")
                    .psm;
                opened.push((psm, source_cid));
            }
        }
        for pdu in pdus {
            let packets = fragment_l2cap_pdu(
                handle,
                0,
                self.acl_flow.payload_size,
                &pdu.to_bytes(false),
                false,
            )?;
            self.acl_flow.enqueue(packets);
        }
        self.acl_flow.flush(&mut self.output);
        for (psm, source_cid) in opened {
            self.output
                .push_back(HostOutput::Event(SessionEvent::ChannelOpened {
                    psm,
                    source_cid,
                }));
        }
        Ok(())
    }

    fn queue_command(&mut self, opcode: u16, parameters: Vec<u8>) -> Result<(), HostError> {
        self.output
            .push_back(HostOutput::Command(CommandPacket::new(opcode, parameters)?));
        Ok(())
    }

    fn connection_handle(&self) -> Result<u16, HostError> {
        self.connection
            .as_ref()
            .map(|connection| connection.handle)
            .ok_or(HostError::NoConnection)
    }

    fn ensure_handle(&self, actual: u16) -> Result<(), HostError> {
        let expected = self.connection_handle()?;
        if actual == expected {
            Ok(())
        } else {
            Err(HostError::ConnectionHandleMismatch { expected, actual })
        }
    }

    fn connection_mut(&mut self, actual: u16) -> Result<&mut Connection, HostError> {
        self.ensure_handle(actual)?;
        self.connection.as_mut().ok_or(HostError::NoConnection)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AddressKind;
    use crate::l2cap::classic::ClassicChannelState;
    use std::collections::HashMap;

    #[derive(Default)]
    struct MemoryBondStore {
        bonds: HashMap<BluetoothAddress, ClassicBond>,
    }

    impl BondStore for MemoryBondStore {
        fn load(&self, peer: BluetoothAddress) -> Result<Option<ClassicBond>, BondStoreError> {
            Ok(self.bonds.get(&peer).cloned())
        }

        fn upsert(
            &mut self,
            peer: BluetoothAddress,
            bond: ClassicBond,
        ) -> Result<(), BondStoreError> {
            self.bonds.insert(peer, bond);
            Ok(())
        }

        fn delete(&mut self, peer: BluetoothAddress) -> Result<(), BondStoreError> {
            self.bonds.remove(&peer);
            Ok(())
        }
    }

    fn peer() -> BluetoothAddress {
        BluetoothAddress::parse("11:22:33:44:55:66", AddressKind::Public).unwrap()
    }

    fn connected_host(total_acl_packets: u16) -> ClassicHost<MemoryBondStore> {
        let mut host = ClassicHost::new(MemoryBondStore::default(), 1024, total_acl_packets);
        host.handle_event(ClassicEvent::ConnectionComplete {
            status: 0,
            connection_handle: 0x0040,
            peer: peer(),
        })
        .unwrap();
        while host.pop_output().is_some() {}
        host
    }

    #[test]
    fn stored_and_new_link_keys_drive_reconnect_without_le_key_state() {
        let peer = peer();
        let existing = ClassicBond::new([0x11; 16], 0x05, true);
        let mut store = MemoryBondStore::default();
        store.bonds.insert(peer, existing.clone());
        let mut host = ClassicHost::new(store, 1024, 4);

        host.handle_event(ClassicEvent::LinkKeyRequest { peer })
            .unwrap();
        let HostOutput::Command(reply) = host.pop_output().unwrap() else {
            panic!("expected link-key reply")
        };
        assert_eq!(reply.opcode, HCI_LINK_KEY_REQUEST_REPLY);
        assert_eq!(&reply.parameters[..6], peer.as_le_bytes());
        assert_eq!(&reply.parameters[6..], existing.link_key());

        host.handle_event(ClassicEvent::LinkKeyNotification {
            peer,
            link_key: [0x22; 16],
            link_key_type: 0x04,
        })
        .unwrap();
        assert_eq!(
            host.bond_store().bonds.get(&peer),
            Some(&ClassicBond::new([0x22; 16], 0x04, false))
        );
        assert_eq!(
            host.pop_output(),
            Some(HostOutput::Event(SessionEvent::BondStored { peer }))
        );
    }

    #[test]
    fn missing_link_key_uses_negative_reply() {
        let peer = peer();
        let mut host = ClassicHost::new(MemoryBondStore::default(), 1024, 4);

        host.handle_event(ClassicEvent::LinkKeyRequest { peer })
            .unwrap();

        let HostOutput::Command(reply) = host.pop_output().unwrap() else {
            panic!("expected negative link-key reply")
        };
        assert_eq!(reply.opcode, HCI_LINK_KEY_REQUEST_NEGATIVE_REPLY);
        assert_eq!(reply.parameters, peer.as_le_bytes());
    }

    #[test]
    fn authentication_transitions_to_encrypted_session() {
        let mut host = connected_host(4);

        host.handle_event(ClassicEvent::AuthenticationComplete {
            status: 0,
            connection_handle: 0x0040,
        })
        .unwrap();
        let HostOutput::Command(command) = host.pop_output().unwrap() else {
            panic!("expected encryption command")
        };
        assert_eq!(command.opcode, HCI_SET_CONNECTION_ENCRYPTION);
        assert_eq!(command.parameters, [0x40, 0x00, 0x01]);

        host.handle_event(ClassicEvent::EncryptionChange {
            status: 0,
            connection_handle: 0x0040,
            enabled: true,
        })
        .unwrap();
        assert_eq!(
            host.pop_output(),
            Some(HostOutput::Event(SessionEvent::Encrypted { peer: peer() }))
        );
    }

    #[test]
    fn incoming_pairing_emits_accept_io_and_confirmation_commands() {
        let peer = peer();
        let mut host = ClassicHost::new(MemoryBondStore::default(), 1024, 4);

        host.handle_event(ClassicEvent::ConnectionRequest { peer })
            .unwrap();
        host.handle_event(ClassicEvent::IoCapabilityRequest { peer })
            .unwrap();
        host.handle_event(ClassicEvent::UserConfirmationRequest { peer })
            .unwrap();

        let opcodes = std::iter::from_fn(|| host.pop_output())
            .map(|output| match output {
                HostOutput::Command(command) => command.opcode,
                other => panic!("unexpected output: {other:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            opcodes,
            [
                HCI_ACCEPT_CONNECTION_REQUEST,
                HCI_IO_CAPABILITY_REQUEST_REPLY,
                HCI_USER_CONFIRMATION_REQUEST_REPLY,
            ]
        );
    }

    #[test]
    fn acl_credit_never_sends_more_than_controller_capacity() {
        let mut host = connected_host(1);
        let pdu = L2capPdu::new(0x0040, vec![0xAA; 20]);
        host.acl_flow.payload_size = 8;
        let packets = fragment_l2cap_pdu(0x0040, 0, 8, &pdu.to_bytes(false), false).unwrap();
        assert_eq!(packets.len(), 3);
        host.acl_flow.enqueue(packets);
        host.acl_flow.flush(&mut host.output);

        assert!(matches!(host.pop_output(), Some(HostOutput::Acl(_))));
        assert!(host.pop_output().is_none());

        host.handle_event(ClassicEvent::NumberOfCompletedPackets {
            connection_handle: 0x0040,
            completed: 1,
        })
        .unwrap();
        assert!(matches!(host.pop_output(), Some(HostOutput::Acl(_))));
        assert!(host.pop_output().is_none());
    }

    #[test]
    fn completed_packet_count_cannot_exceed_in_flight_acl() {
        let mut host = connected_host(1);

        let error = host
            .handle_event(ClassicEvent::NumberOfCompletedPackets {
                connection_handle: 0x0040,
                completed: 1,
            })
            .unwrap_err();

        assert!(matches!(
            error,
            HostError::AclCreditUnderflow {
                in_flight: 0,
                completed: 1
            }
        ));
    }

    #[test]
    fn pending_connection_rejects_a_different_peer() {
        let expected = peer();
        let actual = BluetoothAddress::parse("22:33:44:55:66:77", AddressKind::Public).unwrap();
        let mut host = ClassicHost::new(MemoryBondStore::default(), 1024, 4);
        host.handle_event(ClassicEvent::ConnectionRequest { peer: expected })
            .unwrap();
        while host.pop_output().is_some() {}

        let error = host
            .handle_event(ClassicEvent::ConnectionComplete {
                status: 0,
                connection_handle: 0x0040,
                peer: actual,
            })
            .unwrap_err();

        assert!(matches!(
            error,
            HostError::PeerMismatch {
                expected: seen_expected,
                actual: seen_actual,
            } if seen_expected == expected && seen_actual == actual
        ));
    }

    #[test]
    fn disconnect_clears_partial_acl_reassembly_for_reused_handle() {
        let mut host = connected_host(4);
        let pdu = L2capPdu::new(0x0040, vec![0xAA; 20]);
        let fragments = fragment_l2cap_pdu(0x0040, 0, 8, &pdu.to_bytes(false), false).unwrap();
        host.process_acl(fragments[0].clone()).unwrap();

        host.handle_event(ClassicEvent::DisconnectionComplete {
            connection_handle: 0x0040,
            reason: 0x13,
        })
        .unwrap();
        while host.pop_output().is_some() {}
        host.handle_event(ClassicEvent::ConnectionComplete {
            status: 0,
            connection_handle: 0x0040,
            peer: peer(),
        })
        .unwrap();
        while host.pop_output().is_some() {}

        let error = host.process_acl(fragments[1].clone()).unwrap_err();
        assert!(matches!(
            error,
            HostError::Codec(CodecError::UnexpectedContinuation {
                connection_handle: 0x0040
            })
        ));
    }

    #[test]
    fn test_only_peer_opens_channel_and_exchanges_sdu() {
        let mut host = connected_host(16);
        host.register_server(0x0011, ClassicChannelSpec { mtu: 128 })
            .unwrap();
        let mut remote = ChannelManager::new();
        let remote_cid = remote
            .connect(0x0011, ClassicChannelSpec { mtu: 96 })
            .unwrap();

        let mut host_cid = None;
        for _ in 0..32 {
            while let Some(pdu) = remote.poll_outbound() {
                host.process_acl(AclPacket::new(0x0040, 0, 0, pdu.to_bytes(false)).unwrap())
                    .unwrap();
            }
            while let Some(output) = host.pop_output() {
                match output {
                    HostOutput::Acl(packet) => remote.process_bytes(&packet.data).unwrap(),
                    HostOutput::Event(SessionEvent::ChannelOpened {
                        psm: 0x0011,
                        source_cid,
                    }) => host_cid = Some(source_cid),
                    _ => {}
                }
            }
            if host_cid.is_some()
                && remote
                    .channel(remote_cid)
                    .is_some_and(|channel| channel.state == ClassicChannelState::Open)
            {
                break;
            }
        }

        let host_cid = host_cid.expect("host accepted control channel");
        remote.send(remote_cid, b"from-peer").unwrap();
        for pdu in remote.drain_outbound() {
            host.process_acl(AclPacket::new(0x0040, 0, 0, pdu.to_bytes(false)).unwrap())
                .unwrap();
        }
        assert_eq!(
            host.take_channel_sdu(host_cid).as_deref(),
            Some(b"from-peer".as_slice())
        );

        host.send_channel_sdu(host_cid, b"from-host").unwrap();
        while let Some(output) = host.pop_output() {
            if let HostOutput::Acl(packet) = output {
                remote.process_bytes(&packet.data).unwrap();
            }
        }
        assert_eq!(
            remote
                .channel_mut(remote_cid)
                .unwrap()
                .pop_received()
                .as_deref(),
            Some(b"from-host".as_slice())
        );
    }
}
