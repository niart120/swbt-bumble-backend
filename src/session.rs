use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use crate::api::{SessionDriver, UsbAdapterMetadata};
use crate::classic_host::{ClassicEvent, ClassicHost, HostOutput, SessionEvent};
use crate::csr::{CsrVendorCommand, matches_csr_vendor_response};
use crate::hci::{CommandPacket, EventPacket, Packet};
use crate::hid_service::{
    HID_CONTROL_PSM, HID_INTERRUPT_PSM, HidSdpChannel, HidpBridge, HidpBridgeError,
    HidpBridgeEvent, SDP_PSM,
};
use crate::identity::{
    AdapterIdentityBackend, AdapterIdentityPreparation, AdapterIdentitySession,
    IdentityPreparationError, IdentityPreparationErrorKind, IdentityPreparationOptions,
    prepare_adapter_identity,
};
use crate::l2cap::classic::ClassicChannelSpec;
use crate::usb::{OpenedUsb, ReaderEvent, UsbSelector};
use crate::{
    ActivityNotifier, AddressKind, BluetoothAddress, BondStore, Capabilities, Channel,
    ControllerVersion, Error, ErrorKind, Event, LocalIdentity, OpenOptions, Session, SessionConfig,
};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const HCI_RESET: u16 = 0x0C03;
const HCI_SET_EVENT_MASK: u16 = 0x0C01;
const HCI_WRITE_LOCAL_NAME: u16 = 0x0C13;
const HCI_WRITE_SCAN_ENABLE: u16 = 0x0C1A;
const HCI_WRITE_CLASS_OF_DEVICE: u16 = 0x0C24;
const HCI_WRITE_EXTENDED_INQUIRY_RESPONSE: u16 = 0x0C52;
const HCI_WRITE_SIMPLE_PAIRING_MODE: u16 = 0x0C56;
const HCI_WRITE_DEFAULT_LINK_POLICY_SETTINGS: u16 = 0x080F;
const HCI_READ_LOCAL_VERSION_INFORMATION: u16 = 0x1001;
const HCI_READ_LOCAL_SUPPORTED_COMMANDS: u16 = 0x1002;
const HCI_READ_LOCAL_EXTENDED_FEATURES: u16 = 0x1004;
const HCI_READ_BUFFER_SIZE: u16 = 0x1005;
const HCI_READ_BD_ADDR: u16 = 0x1009;
const HCI_LE_SET_EVENT_MASK: u16 = 0x2001;
const HCI_CREATE_CONNECTION: u16 = 0x0405;
const HCI_DISCONNECT: u16 = 0x0406;
const HCI_REJECT_CONNECTION_REQUEST: u16 = 0x040A;
const EVENT_CONNECTION_COMPLETE: u8 = 0x03;
const EVENT_CONNECTION_REQUEST: u8 = 0x04;
const EVENT_DISCONNECTION_COMPLETE: u8 = 0x05;
const EVENT_AUTHENTICATION_COMPLETE: u8 = 0x06;
const EVENT_ENCRYPTION_CHANGE: u8 = 0x08;
const EVENT_COMMAND_COMPLETE: u8 = 0x0E;
const EVENT_COMMAND_STATUS: u8 = 0x0F;
const EVENT_NUMBER_OF_COMPLETED_PACKETS: u8 = 0x13;
const EVENT_LINK_KEY_REQUEST: u8 = 0x17;
const EVENT_LINK_KEY_NOTIFICATION: u8 = 0x18;
const EVENT_IO_CAPABILITY_REQUEST: u8 = 0x31;
const EVENT_USER_CONFIRMATION_REQUEST: u8 = 0x33;
const EVENT_VENDOR: u8 = 0xFF;
const EVENT_MASK: [u8; 8] = [0xFF, 0x9F, 0xFF, 0xBF, 0x07, 0xF8, 0xBF, 0x3D];
const LE_EVENT_MASK: [u8; 8] = [0xFF, 0xFF, 0xF7, 0xFF, 0x0F, 0xED, 0x7B, 0x00];
const DEFAULT_LINK_POLICY_SETTINGS: u16 = 0x0005;
const CONNECTION_REJECTED_UNACCEPTABLE_ADDRESS: u8 = 0x0F;
const REMOTE_USER_TERMINATED_CONNECTION: u8 = 0x13;
const AUTHENTICATION_FAILURE: u8 = 0x05;
const BR_EDR_NOT_SUPPORTED_MASK: u8 = 0x20;
const CLASSIC_SERVER_MTU: u16 = 672;
const EVENT_QUEUE_CAPACITY: usize = 64;

trait HciIo: Send {
    fn send(&mut self, packet: &Packet) -> Result<(), Error>;
    fn recv_timeout(&mut self, timeout: Duration) -> Result<Option<Packet>, Error>;
    fn metadata(&self) -> UsbAdapterMetadata;
    fn close(&mut self) -> Result<(), Error>;
}

struct UsbHciIo {
    opened: OpenedUsb,
}

impl HciIo for UsbHciIo {
    fn send(&mut self, packet: &Packet) -> Result<(), Error> {
        self.opened
            .send(packet)
            .map_err(|source| Error::with_source(ErrorKind::SourceTerminated, source))
    }

    fn recv_timeout(&mut self, timeout: Duration) -> Result<Option<Packet>, Error> {
        match self.opened.recv_timeout(timeout) {
            Ok(ReaderEvent::Packet(packet)) => Ok(Some(packet)),
            Ok(ReaderEvent::Failed(source)) => {
                Err(Error::with_source(ErrorKind::SourceTerminated, source))
            }
            Ok(ReaderEvent::Ended) | Err(RecvTimeoutError::Disconnected) => {
                Err(Error::new(ErrorKind::SourceTerminated))
            }
            Err(RecvTimeoutError::Timeout) => Ok(None),
        }
    }

    fn metadata(&self) -> UsbAdapterMetadata {
        let metadata = self.opened.metadata();
        UsbAdapterMetadata {
            vendor_id: metadata.vendor_id(),
            product_id: metadata.product_id(),
            bus: metadata.bus(),
            device_address: metadata.device_address(),
            ports: metadata.ports().into(),
        }
    }

    fn close(&mut self) -> Result<(), Error> {
        self.opened
            .close()
            .map_err(|source| Error::with_source(ErrorKind::CloseFailed, source))
    }
}

struct UsbIdentityBackend {
    selector: UsbSelector,
    activity: ActivityNotifier,
    origin: Instant,
}

impl UsbIdentityBackend {
    fn new(selector: UsbSelector, activity: ActivityNotifier) -> Self {
        Self {
            selector,
            activity,
            origin: Instant::now(),
        }
    }
}

impl AdapterIdentityBackend for UsbIdentityBackend {
    type Error = Error;
    type Session = UsbIdentitySession;

    fn open(&mut self) -> Result<Self::Session, Self::Error> {
        let opened = crate::usb::open(&self.selector, self.activity.clone())
            .map_err(|source| Error::with_source(ErrorKind::OpenFailed, source))?;
        Ok(UsbIdentitySession {
            io: Some(UsbHciIo { opened }),
        })
    }

    fn now(&self) -> Duration {
        self.origin.elapsed()
    }

    fn sleep(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

struct UsbIdentitySession {
    io: Option<UsbHciIo>,
}

impl UsbIdentitySession {
    fn io_mut(&mut self) -> Result<&mut UsbHciIo, Error> {
        self.io
            .as_mut()
            .ok_or_else(|| Error::new(ErrorKind::Closed))
    }
}

impl AdapterIdentitySession for UsbIdentitySession {
    type Error = Error;

    fn initialize(&mut self, _response_timeout: Duration) -> Result<u16, Self::Error> {
        let response = send_complete(
            self.io_mut()?,
            command(HCI_READ_LOCAL_VERSION_INFORMATION, Vec::new())?,
        )?;
        Ok(parse_version(&response)?.company_identifier)
    }

    fn read_address(&mut self, _response_timeout: Duration) -> Result<[u8; 6], Self::Error> {
        let response = send_complete(self.io_mut()?, command(HCI_READ_BD_ADDR, Vec::new())?)?;
        require_len(&response, 6, "local address")?;
        let mut display_order: [u8; 6] = response
            .try_into()
            .expect("six-byte response length was checked");
        display_order.reverse();
        Ok(display_order)
    }

    fn send_vendor_command(
        &mut self,
        command: &CsrVendorCommand,
        response_timeout: Duration,
    ) -> Result<Box<[u8]>, Self::Error> {
        let packet = CommandPacket::new(command.op_code(), command.parameters().to_vec())
            .map_err(|source| Error::with_source(ErrorKind::ProtocolViolation, source))?;
        self.io_mut()?.send(&Packet::Command(packet))?;
        loop {
            let packet = self
                .io_mut()?
                .recv_timeout(response_timeout)?
                .ok_or_else(|| Error::new(ErrorKind::OpenFailed))?;
            let Packet::Event(event) = packet else {
                continue;
            };
            if event.event_code == EVENT_VENDOR
                && matches_csr_vendor_response(command, &event.parameters)
            {
                return Ok(event.parameters.into_boxed_slice());
            }
            if event.event_code == EVENT_COMMAND_STATUS {
                require_len(&event.parameters, 4, "command status")?;
                let opcode = u16::from_le_bytes([event.parameters[2], event.parameters[3]]);
                if opcode == command.op_code() && event.parameters[0] != 0 {
                    return Err(Error::with_source(
                        ErrorKind::OpenFailed,
                        CommandFailure {
                            opcode,
                            status: event.parameters[0],
                        },
                    ));
                }
            }
        }
    }

    fn send_command_without_response(
        &mut self,
        command: &CsrVendorCommand,
    ) -> Result<(), Self::Error> {
        let packet = CommandPacket::new(command.op_code(), command.parameters().to_vec())
            .map_err(|source| Error::with_source(ErrorKind::ProtocolViolation, source))?;
        self.io_mut()?.send(&Packet::Command(packet))
    }

    fn close(&mut self) -> Result<(), Self::Error> {
        let Some(mut io) = self.io.take() else {
            return Ok(());
        };
        io.close()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConnectionWindow {
    Idle,
    Pairing { peer: Option<BluetoothAddress> },
    Reconnecting { peer: BluetoothAddress },
    Connected,
}

struct BackendSession<T: HciIo, B: BondStore> {
    io: Option<T>,
    capabilities: Capabilities,
    config: SessionConfig,
    host: ClassicHost<B>,
    window: ConnectionWindow,
    connection: Option<(BluetoothAddress, u16)>,
    rejected_connections: BTreeSet<u16>,
    protocols: ProtocolState,
    active_reconnect: bool,
    terminal: Option<Error>,
    closed: bool,
    pending_events: VecDeque<Event>,
}

struct ProtocolState {
    sdp: BTreeMap<u16, HidSdpChannel>,
    control: Option<ProtocolChannel>,
    interrupt: Option<ProtocolChannel>,
    hidp: HidpBridge,
}

impl Default for ProtocolState {
    fn default() -> Self {
        Self {
            sdp: BTreeMap::new(),
            control: None,
            interrupt: None,
            hidp: HidpBridge::new(0, 0),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct ProtocolChannel {
    cid: u16,
    open: bool,
}

impl<T: HciIo + 'static, B: BondStore + 'static> BackendSession<T, B> {
    fn initialize(mut io: T, config: SessionConfig, bonds: B) -> Result<Self, Error> {
        let initialized = initialize_controller(&mut io, &config)?;
        let capabilities = Capabilities::new(
            initialized.local_address,
            initialized.version,
            io.metadata(),
        );
        let mut host = ClassicHost::new(
            bonds,
            usize::from(initialized.acl_packet_length),
            initialized.acl_packet_count,
        );
        for psm in [SDP_PSM, HID_CONTROL_PSM, HID_INTERRUPT_PSM] {
            host.register_server(
                psm,
                ClassicChannelSpec {
                    mtu: CLASSIC_SERVER_MTU,
                },
            )
            .map_err(|source| Error::with_source(ErrorKind::OpenFailed, source))?;
        }
        Ok(Self {
            io: Some(io),
            capabilities,
            config,
            host,
            window: ConnectionWindow::Idle,
            connection: None,
            rejected_connections: BTreeSet::new(),
            protocols: ProtocolState::default(),
            active_reconnect: false,
            terminal: None,
            closed: false,
            pending_events: VecDeque::new(),
        })
    }

    fn ensure_active(&self) -> Result<(), Error> {
        if self.closed {
            Err(Error::new(ErrorKind::Closed))
        } else if let Some(error) = &self.terminal {
            Err(error.clone())
        } else {
            Ok(())
        }
    }

    fn io_mut(&mut self) -> Result<&mut T, Error> {
        self.io
            .as_mut()
            .ok_or_else(|| Error::new(ErrorKind::Closed))
    }

    fn send_command(&mut self, opcode: u16, parameters: Vec<u8>) -> Result<(), Error> {
        let command = CommandPacket::new(opcode, parameters)
            .map_err(|source| Error::with_source(ErrorKind::ProtocolViolation, source))?;
        self.io_mut()?.send(&Packet::Command(command))
    }

    fn set_scan(&mut self, discoverable: bool, connectable: bool) -> Result<(), Error> {
        let scan_enable = u8::from(discoverable) | (u8::from(connectable) << 1);
        self.send_command(HCI_WRITE_SCAN_ENABLE, vec![scan_enable])
    }

    fn write_inquiry_response(&mut self) -> Result<(), Error> {
        let mut parameters = Vec::with_capacity(241);
        parameters.push(0);
        parameters.extend_from_slice(self.config.extended_inquiry_response());
        self.send_command(HCI_WRITE_EXTENDED_INQUIRY_RESPONSE, parameters)
    }

    fn end_connection_window(&mut self) -> Result<(), Error> {
        if self.window == ConnectionWindow::Idle {
            return Ok(());
        }
        self.write_inquiry_response()?;
        self.set_scan(false, true)?;
        self.set_scan(false, false)?;
        self.window = ConnectionWindow::Idle;
        Ok(())
    }

    fn record_terminal(&mut self, error: Error) -> Error {
        self.terminal = Some(error.clone());
        error
    }

    fn handle_packet(&mut self, packet: Packet) -> Result<(), Error> {
        match packet {
            Packet::Event(event) => self.handle_event(event),
            Packet::Acl(packet) => {
                self.host.process_acl(packet).map_err(map_host_error)?;
                self.drain_host_output()?;
                self.refresh_outgoing_channels()?;
                self.process_protocol_input()?;
                self.drain_host_output()
            }
            Packet::Command(_) => Err(Error::new(ErrorKind::ProtocolViolation)),
        }
    }

    fn handle_polled_packet(&mut self, packet: Packet) -> Result<(), Error> {
        if let Err(error) = self.handle_packet(packet) {
            return Err(self.record_terminal(error));
        }
        if let Some(error) = &self.terminal {
            return Err(error.clone());
        }
        Ok(())
    }

    fn handle_event(&mut self, event: EventPacket) -> Result<(), Error> {
        if event.event_code == EVENT_CONNECTION_REQUEST {
            require_len(&event.parameters, 10, "connection request")?;
            let peer = address_at(&event.parameters, 0)?;
            let accepted = event.parameters[9] == 0x01
                && match &mut self.window {
                    ConnectionWindow::Pairing { peer: latched } => match latched {
                        Some(expected) => *expected == peer,
                        None => {
                            *latched = Some(peer);
                            true
                        }
                    },
                    ConnectionWindow::Reconnecting { peer: expected } => *expected == peer,
                    ConnectionWindow::Idle | ConnectionWindow::Connected => false,
                };
            if !accepted {
                let mut parameters = peer.as_le_bytes().to_vec();
                parameters.push(CONNECTION_REJECTED_UNACCEPTABLE_ADDRESS);
                return self.send_command(HCI_REJECT_CONNECTION_REQUEST, parameters);
            }
        }

        if event.event_code == EVENT_CONNECTION_COMPLETE {
            require_len(&event.parameters, 11, "connection complete")?;
            let status = event.parameters[0];
            let handle = u16::from_le_bytes([event.parameters[1], event.parameters[2]]);
            let peer = address_at(&event.parameters, 3)?;
            let expected = match self.window {
                ConnectionWindow::Pairing { peer } => peer,
                ConnectionWindow::Reconnecting { peer } => Some(peer),
                ConnectionWindow::Idle | ConnectionWindow::Connected => None,
            };
            if status == 0 && (expected != Some(peer) || event.parameters[9] != 0x01) {
                let mut parameters = handle.to_le_bytes().to_vec();
                parameters.push(AUTHENTICATION_FAILURE);
                self.send_command(HCI_DISCONNECT, parameters)?;
                self.rejected_connections.insert(handle);
                return Ok(());
            }
            if status != 0 {
                self.end_connection_window()?;
                self.active_reconnect = false;
                self.enqueue_event(Event::Disconnected {
                    reason: Some(status),
                });
            }
        }

        if event.event_code == EVENT_DISCONNECTION_COMPLETE {
            require_len(&event.parameters, 4, "disconnection complete")?;
            let handle = u16::from_le_bytes([event.parameters[1], event.parameters[2]]);
            if event.parameters[0] == 0 && self.rejected_connections.remove(&handle) {
                return Ok(());
            }
        }

        for classic_event in decode_classic_event(event)? {
            self.host
                .handle_event(classic_event)
                .map_err(map_host_error)?;
        }
        self.drain_host_output()
    }

    fn drain_host_output(&mut self) -> Result<(), Error> {
        while let Some(output) = self.host.pop_output() {
            match output {
                HostOutput::Command(command) => self.io_mut()?.send(&Packet::Command(command))?,
                HostOutput::Acl(packet) => self.io_mut()?.send(&Packet::Acl(packet))?,
                HostOutput::Event(SessionEvent::Connected {
                    peer,
                    connection_handle,
                }) => {
                    self.connection = Some((peer, connection_handle));
                    self.window = ConnectionWindow::Connected;
                    self.protocols = ProtocolState::default();
                    self.write_inquiry_response()?;
                    self.set_scan(false, true)?;
                    self.set_scan(false, false)?;
                    self.enqueue_event(Event::Connected { peer });
                }
                HostOutput::Event(SessionEvent::Disconnected { reason, .. }) => {
                    self.connection = None;
                    self.window = ConnectionWindow::Idle;
                    self.protocols = ProtocolState::default();
                    self.active_reconnect = false;
                    self.enqueue_event(Event::Disconnected {
                        reason: Some(reason),
                    });
                }
                HostOutput::Event(SessionEvent::Encrypted { .. }) => {
                    self.start_active_reconnect_control()?;
                }
                HostOutput::Event(SessionEvent::ChannelOpened { psm, source_cid }) => {
                    self.on_channel_opened(psm, source_cid)?;
                }
                HostOutput::Event(SessionEvent::BondStored { .. }) => {}
            }
        }
        Ok(())
    }

    fn enqueue_event(&mut self, event: Event) {
        if self.terminal.is_some() {
            return;
        }
        if self.pending_events.len() == EVENT_QUEUE_CAPACITY {
            self.terminal = Some(Error::new(ErrorKind::EventQueueOverflow));
            return;
        }
        self.pending_events.push_back(event);
    }

    fn start_active_reconnect_control(&mut self) -> Result<(), Error> {
        if !self.active_reconnect || self.protocols.control.is_some() {
            return Ok(());
        }
        let cid = self
            .host
            .connect_channel(
                HID_CONTROL_PSM,
                ClassicChannelSpec {
                    mtu: CLASSIC_SERVER_MTU,
                },
            )
            .map_err(map_host_error)?;
        self.protocols.control = Some(ProtocolChannel { cid, open: false });
        Ok(())
    }

    fn refresh_outgoing_channels(&mut self) -> Result<(), Error> {
        let control = self.protocols.control.filter(|channel| !channel.open);
        if let Some(channel) = control
            && self
                .host
                .channel_info(channel.cid)
                .is_some_and(|(_, _, open)| open)
        {
            self.on_channel_opened(HID_CONTROL_PSM, channel.cid)?;
        }

        let interrupt = self.protocols.interrupt.filter(|channel| !channel.open);
        if let Some(channel) = interrupt
            && self
                .host
                .channel_info(channel.cid)
                .is_some_and(|(_, _, open)| open)
        {
            self.on_channel_opened(HID_INTERRUPT_PSM, channel.cid)?;
        }
        Ok(())
    }

    fn on_channel_opened(&mut self, psm: u32, cid: u16) -> Result<(), Error> {
        let (actual_psm, peer_mtu, open) = self
            .host
            .channel_info(cid)
            .ok_or_else(|| Error::new(ErrorKind::ProtocolViolation))?;
        if !open || actual_psm != psm {
            return Err(Error::new(ErrorKind::ProtocolViolation));
        }
        match psm {
            SDP_PSM => {
                self.protocols
                    .sdp
                    .entry(cid)
                    .or_insert_with(|| HidSdpChannel::new(self.config.hid_service(), peer_mtu));
            }
            HID_CONTROL_PSM => {
                let first_open = self
                    .protocols
                    .control
                    .is_none_or(|channel| channel.cid == cid && !channel.open);
                if first_open {
                    self.protocols.control = Some(ProtocolChannel { cid, open: true });
                    self.protocols
                        .hidp
                        .set_peer_mtu(Channel::Control, usize::from(peer_mtu));
                    self.enqueue_event(Event::ChannelOpened {
                        channel: Channel::Control,
                    });
                }
                if self.active_reconnect && self.protocols.interrupt.is_none() {
                    let cid = self
                        .host
                        .connect_channel(
                            HID_INTERRUPT_PSM,
                            ClassicChannelSpec {
                                mtu: CLASSIC_SERVER_MTU,
                            },
                        )
                        .map_err(map_host_error)?;
                    self.protocols.interrupt = Some(ProtocolChannel { cid, open: false });
                }
            }
            HID_INTERRUPT_PSM => {
                let first_open = self
                    .protocols
                    .interrupt
                    .is_none_or(|channel| channel.cid == cid && !channel.open);
                if first_open {
                    self.protocols.interrupt = Some(ProtocolChannel { cid, open: true });
                    self.protocols
                        .hidp
                        .set_peer_mtu(Channel::Interrupt, usize::from(peer_mtu));
                    self.enqueue_event(Event::ChannelOpened {
                        channel: Channel::Interrupt,
                    });
                }
            }
            _ => return Err(Error::new(ErrorKind::ProtocolViolation)),
        }
        Ok(())
    }

    fn process_protocol_input(&mut self) -> Result<(), Error> {
        let sdp_cids = self.protocols.sdp.keys().copied().collect::<Vec<_>>();
        for cid in sdp_cids {
            while let Some(request) = self.host.take_channel_sdu(cid) {
                let response = self
                    .protocols
                    .sdp
                    .get_mut(&cid)
                    .and_then(|channel| channel.handle_sdu(&request));
                if let Some(response) = response {
                    self.host
                        .send_channel_sdu(cid, &response)
                        .map_err(map_host_error)?;
                }
            }
        }

        for (channel, protocol_channel) in [
            (Channel::Control, self.protocols.control),
            (Channel::Interrupt, self.protocols.interrupt),
        ] {
            let Some(protocol_channel) = protocol_channel.filter(|channel| channel.open) else {
                continue;
            };
            while let Some(sdu) = self.host.take_channel_sdu(protocol_channel.cid) {
                match self.protocols.hidp.handle(channel, &sdu) {
                    Ok(events) => self.apply_hidp_events(events)?,
                    Err(HidpBridgeError::Malformed {
                        channel: Channel::Control,
                    }) => {
                        if let Ok(response) = self.protocols.hidp.invalid_parameter_response() {
                            self.host
                                .send_channel_sdu(protocol_channel.cid, &response)
                                .map_err(map_host_error)?;
                        }
                    }
                    Err(_) => {}
                }
            }
        }
        Ok(())
    }

    fn apply_hidp_events(&mut self, events: Vec<HidpBridgeEvent>) -> Result<(), Error> {
        for event in events {
            match event {
                HidpBridgeEvent::Output { channel, payload } => {
                    self.enqueue_event(Event::HidOutput { channel, payload });
                }
                HidpBridgeEvent::ControlResponse(response) => {
                    if let Some(control) = self.protocols.control.filter(|channel| channel.open) {
                        self.host
                            .send_channel_sdu(control.cid, &response)
                            .map_err(map_host_error)?;
                    }
                }
                HidpBridgeEvent::Suspend
                | HidpBridgeEvent::Resume
                | HidpBridgeEvent::VirtualCableUnplug
                | HidpBridgeEvent::Unsupported { .. } => {}
            }
        }
        Ok(())
    }
}

impl<T: HciIo + 'static, B: BondStore + 'static> SessionDriver for BackendSession<T, B> {
    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    fn start_pairing(&mut self) -> Result<(), Error> {
        self.ensure_active()?;
        match self.window {
            ConnectionWindow::Pairing { .. } => return Ok(()),
            ConnectionWindow::Idle => {}
            ConnectionWindow::Reconnecting { .. } | ConnectionWindow::Connected => {
                return Err(Error::new(ErrorKind::SendRejected));
            }
        }
        self.set_scan(false, true)?;
        self.write_inquiry_response()?;
        self.set_scan(true, true)?;
        self.active_reconnect = false;
        self.window = ConnectionWindow::Pairing { peer: None };
        Ok(())
    }

    fn start_reconnect(&mut self) -> Result<(), Error> {
        self.ensure_active()?;
        match self.window {
            ConnectionWindow::Reconnecting { .. } => return Ok(()),
            ConnectionWindow::Idle => {}
            ConnectionWindow::Pairing { .. } | ConnectionWindow::Connected => {
                return Err(Error::new(ErrorKind::SendRejected));
            }
        }
        let bonds = self
            .host
            .bond_store()
            .load_all()
            .map_err(|source| Error::with_source(ErrorKind::InvalidBondStore, source))?;
        let [(peer, _bond)] = bonds.as_slice() else {
            return Err(Error::new(if bonds.is_empty() {
                ErrorKind::NoBond
            } else {
                ErrorKind::InvalidBondStore
            }));
        };
        let peer = *peer;
        self.set_scan(false, true)?;
        self.write_inquiry_response()?;
        self.set_scan(false, true)?;
        let mut parameters = peer.as_le_bytes().to_vec();
        parameters.extend_from_slice(&0_u16.to_le_bytes());
        parameters.extend_from_slice(&[0, 0]);
        parameters.extend_from_slice(&0_u16.to_le_bytes());
        parameters.push(1);
        self.send_command(HCI_CREATE_CONNECTION, parameters)?;
        self.active_reconnect = true;
        self.window = ConnectionWindow::Reconnecting { peer };
        Ok(())
    }

    fn poll(&mut self, timeout: Duration) -> Result<Vec<Event>, Error> {
        self.ensure_active()?;
        if self.pending_events.is_empty() {
            let received = self.io_mut()?.recv_timeout(timeout);
            match received {
                Ok(Some(packet)) => self.handle_polled_packet(packet)?,
                Ok(None) => {}
                Err(error) => return Err(self.record_terminal(error)),
            }
        }
        loop {
            let received = self.io_mut()?.recv_timeout(Duration::ZERO);
            match received {
                Ok(Some(packet)) => self.handle_polled_packet(packet)?,
                Ok(None) => break,
                Err(error) => return Err(self.record_terminal(error)),
            }
        }
        Ok(self.pending_events.drain(..).collect())
    }

    fn interrupt_send_capacity_available(&self) -> bool {
        self.terminal.is_none()
            && !self.closed
            && self.protocols.interrupt.is_some_and(|channel| channel.open)
            && self.host.interrupt_send_capacity_available()
    }

    fn send_interrupt(&mut self, payload: &[u8]) -> Result<(), Error> {
        self.ensure_active()?;
        let Some(channel) = self.protocols.interrupt.filter(|channel| channel.open) else {
            return Err(Error::new(ErrorKind::SendRejected));
        };
        if !self.host.interrupt_send_capacity_available() {
            return Err(Error::new(ErrorKind::SendRejected));
        }
        let encoded = self
            .protocols
            .hidp
            .encode_input(payload)
            .map_err(|source| Error::with_source(ErrorKind::SendRejected, source))?;
        self.host
            .send_channel_sdu(channel.cid, &encoded)
            .map_err(|source| Error::with_source(ErrorKind::SendRejected, source))?;
        self.drain_host_output()
    }

    fn drain_interrupt(&mut self, timeout: Duration) -> Result<(), Error> {
        self.ensure_active()?;
        let deadline = Instant::now()
            .checked_add(timeout)
            .unwrap_or_else(Instant::now);
        loop {
            if self.host.channel_output_is_flushed() {
                return Ok(());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(Error::new(ErrorKind::DrainTimedOut));
            }
            let received = self.io_mut()?.recv_timeout(remaining);
            match received {
                Ok(Some(packet)) => self.handle_polled_packet(packet)?,
                Ok(None) => return Err(Error::new(ErrorKind::DrainTimedOut)),
                Err(error) => return Err(self.record_terminal(error)),
            }
        }
    }

    fn disconnect(&mut self) -> Result<(), Error> {
        self.ensure_active()?;
        self.protocols = ProtocolState::default();
        self.active_reconnect = false;
        if let Some((_, handle)) = self.connection.take() {
            let mut parameters = handle.to_le_bytes().to_vec();
            parameters.push(REMOTE_USER_TERMINATED_CONNECTION);
            self.send_command(HCI_DISCONNECT, parameters)?;
        }
        self.end_connection_window()
    }

    fn close(&mut self) -> Result<(), Error> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        self.connection = None;
        self.rejected_connections.clear();
        self.window = ConnectionWindow::Idle;
        self.protocols = ProtocolState::default();
        self.active_reconnect = false;
        self.pending_events.clear();
        let Some(mut io) = self.io.take() else {
            return Ok(());
        };
        let result = io.close();
        drop(io);
        result
    }
}

struct InitializedController {
    local_address: BluetoothAddress,
    version: ControllerVersion,
    acl_packet_length: u16,
    acl_packet_count: u16,
}

fn initialize_controller<T: HciIo>(
    io: &mut T,
    config: &SessionConfig,
) -> Result<InitializedController, Error> {
    send_complete(io, command(HCI_RESET, Vec::new())?)?;
    let supported_commands =
        send_complete(io, command(HCI_READ_LOCAL_SUPPORTED_COMMANDS, Vec::new())?)?;
    require_len(&supported_commands, 64, "supported commands")?;

    let version = parse_version(&send_complete(
        io,
        command(HCI_READ_LOCAL_VERSION_INFORMATION, Vec::new())?,
    )?)?;
    let extended_features = send_complete(io, command(HCI_READ_LOCAL_EXTENDED_FEATURES, vec![0])?)?;
    require_len(&extended_features, 10, "extended features")?;
    if extended_features[0] != 0 || extended_features[2 + 4] & BR_EDR_NOT_SUPPORTED_MASK != 0 {
        return Err(Error::new(ErrorKind::UnsupportedController));
    }

    send_complete(io, command(HCI_SET_EVENT_MASK, EVENT_MASK.to_vec())?)?;
    send_complete(io, command(HCI_LE_SET_EVENT_MASK, LE_EVENT_MASK.to_vec())?)?;
    let buffer = send_complete(io, command(HCI_READ_BUFFER_SIZE, Vec::new())?)?;
    require_len(&buffer, 7, "buffer size")?;
    let acl_packet_length = u16::from_le_bytes([buffer[0], buffer[1]]);
    let acl_packet_count = u16::from_le_bytes([buffer[3], buffer[4]]);
    if acl_packet_length == 0 || acl_packet_count == 0 {
        return Err(Error::new(ErrorKind::UnsupportedController));
    }

    let address = send_complete(io, command(HCI_READ_BD_ADDR, Vec::new())?)?;
    require_len(&address, 6, "local address")?;
    let local_address = BluetoothAddress::from_le_bytes(
        address
            .as_slice()
            .try_into()
            .expect("six-byte response length was checked"),
        AddressKind::Public,
    );
    if local_address.as_le_bytes() == &[0; 6] {
        return Err(Error::new(ErrorKind::InvalidControllerIdentity));
    }

    for command in identity_commands(config)? {
        send_complete(io, command)?;
    }

    Ok(InitializedController {
        local_address,
        version,
        acl_packet_length,
        acl_packet_count,
    })
}

fn send_complete<T: HciIo>(io: &mut T, command: CommandPacket) -> Result<Vec<u8>, Error> {
    let opcode = command.opcode;
    io.send(&Packet::Command(command))?;
    loop {
        let packet = io
            .recv_timeout(COMMAND_TIMEOUT)?
            .ok_or_else(|| Error::new(ErrorKind::OpenFailed))?;
        let Packet::Event(event) = packet else {
            continue;
        };
        match event.event_code {
            EVENT_COMMAND_COMPLETE => {
                require_min_len(&event.parameters, 4, "command complete")?;
                let completed_opcode =
                    u16::from_le_bytes([event.parameters[1], event.parameters[2]]);
                if completed_opcode != opcode {
                    continue;
                }
                if event.parameters[3] != 0 {
                    return Err(Error::with_source(
                        ErrorKind::OpenFailed,
                        CommandFailure {
                            opcode,
                            status: event.parameters[3],
                        },
                    ));
                }
                return Ok(event.parameters[4..].to_vec());
            }
            EVENT_COMMAND_STATUS => {
                require_len(&event.parameters, 4, "command status")?;
                let completed_opcode =
                    u16::from_le_bytes([event.parameters[2], event.parameters[3]]);
                if completed_opcode == opcode && event.parameters[0] != 0 {
                    return Err(Error::with_source(
                        ErrorKind::OpenFailed,
                        CommandFailure {
                            opcode,
                            status: event.parameters[0],
                        },
                    ));
                }
            }
            _ => {}
        }
    }
}

fn parse_version(parameters: &[u8]) -> Result<ControllerVersion, Error> {
    require_len(parameters, 8, "local version")?;
    Ok(ControllerVersion {
        hci_version: parameters[0],
        hci_subversion: u16::from_le_bytes([parameters[1], parameters[2]]),
        lmp_version: parameters[3],
        company_identifier: u16::from_le_bytes([parameters[4], parameters[5]]),
        lmp_subversion: u16::from_le_bytes([parameters[6], parameters[7]]),
    })
}

fn identity_commands(config: &SessionConfig) -> Result<[CommandPacket; 6], Error> {
    let mut local_name = vec![0; 248];
    local_name[..config.local_name().len()].copy_from_slice(config.local_name().as_bytes());
    let mut class_of_device = config.class_of_device().to_le_bytes().to_vec();
    class_of_device.truncate(3);
    let mut inquiry_response = Vec::with_capacity(241);
    inquiry_response.push(0);
    inquiry_response.extend_from_slice(config.extended_inquiry_response());
    Ok([
        command(HCI_WRITE_LOCAL_NAME, local_name)?,
        command(HCI_WRITE_CLASS_OF_DEVICE, class_of_device)?,
        command(HCI_WRITE_SIMPLE_PAIRING_MODE, vec![1])?,
        command(HCI_WRITE_EXTENDED_INQUIRY_RESPONSE, inquiry_response)?,
        command(
            HCI_WRITE_DEFAULT_LINK_POLICY_SETTINGS,
            DEFAULT_LINK_POLICY_SETTINGS.to_le_bytes().to_vec(),
        )?,
        command(HCI_WRITE_SCAN_ENABLE, vec![0])?,
    ])
}

fn command(opcode: u16, parameters: Vec<u8>) -> Result<CommandPacket, Error> {
    CommandPacket::new(opcode, parameters)
        .map_err(|source| Error::with_source(ErrorKind::InvalidConfiguration, source))
}

fn decode_classic_event(event: EventPacket) -> Result<Vec<ClassicEvent>, Error> {
    let parameters = event.parameters;
    let events = match event.event_code {
        EVENT_CONNECTION_REQUEST => {
            require_len(&parameters, 10, "connection request")?;
            vec![ClassicEvent::ConnectionRequest {
                peer: address_at(&parameters, 0)?,
            }]
        }
        EVENT_CONNECTION_COMPLETE => {
            require_len(&parameters, 11, "connection complete")?;
            vec![ClassicEvent::ConnectionComplete {
                status: parameters[0],
                connection_handle: u16::from_le_bytes([parameters[1], parameters[2]]),
                peer: address_at(&parameters, 3)?,
            }]
        }
        EVENT_LINK_KEY_REQUEST => {
            require_len(&parameters, 6, "link key request")?;
            vec![ClassicEvent::LinkKeyRequest {
                peer: address_at(&parameters, 0)?,
            }]
        }
        EVENT_LINK_KEY_NOTIFICATION => {
            require_len(&parameters, 23, "link key notification")?;
            vec![ClassicEvent::LinkKeyNotification {
                peer: address_at(&parameters, 0)?,
                link_key: parameters[6..22]
                    .try_into()
                    .expect("link-key response length was checked"),
                link_key_type: parameters[22],
            }]
        }
        EVENT_IO_CAPABILITY_REQUEST => {
            require_len(&parameters, 6, "IO capability request")?;
            vec![ClassicEvent::IoCapabilityRequest {
                peer: address_at(&parameters, 0)?,
            }]
        }
        EVENT_USER_CONFIRMATION_REQUEST => {
            require_len(&parameters, 10, "user confirmation request")?;
            vec![ClassicEvent::UserConfirmationRequest {
                peer: address_at(&parameters, 0)?,
            }]
        }
        EVENT_AUTHENTICATION_COMPLETE => {
            require_len(&parameters, 3, "authentication complete")?;
            vec![ClassicEvent::AuthenticationComplete {
                status: parameters[0],
                connection_handle: u16::from_le_bytes([parameters[1], parameters[2]]),
            }]
        }
        EVENT_ENCRYPTION_CHANGE => {
            require_len(&parameters, 4, "encryption change")?;
            vec![ClassicEvent::EncryptionChange {
                status: parameters[0],
                connection_handle: u16::from_le_bytes([parameters[1], parameters[2]]),
                enabled: parameters[3] != 0,
            }]
        }
        EVENT_NUMBER_OF_COMPLETED_PACKETS => completed_packet_events(&parameters)?,
        EVENT_DISCONNECTION_COMPLETE => {
            require_len(&parameters, 4, "disconnection complete")?;
            if parameters[0] == 0 {
                vec![ClassicEvent::DisconnectionComplete {
                    connection_handle: u16::from_le_bytes([parameters[1], parameters[2]]),
                    reason: parameters[3],
                }]
            } else {
                Vec::new()
            }
        }
        _ => Vec::new(),
    };
    Ok(events)
}

fn completed_packet_events(parameters: &[u8]) -> Result<Vec<ClassicEvent>, Error> {
    let Some((&count, entries)) = parameters.split_first() else {
        return Err(protocol_error("completed packets"));
    };
    require_len(entries, usize::from(count) * 4, "completed packets")?;
    Ok(entries
        .chunks_exact(4)
        .map(|entry| ClassicEvent::NumberOfCompletedPackets {
            connection_handle: u16::from_le_bytes([entry[0], entry[1]]),
            completed: u16::from_le_bytes([entry[2], entry[3]]),
        })
        .collect())
}

fn address_at(parameters: &[u8], offset: usize) -> Result<BluetoothAddress, Error> {
    let bytes = parameters
        .get(offset..offset + 6)
        .ok_or_else(|| protocol_error("Bluetooth address"))?;
    Ok(BluetoothAddress::from_le_bytes(
        bytes
            .try_into()
            .expect("six-byte address slice was checked"),
        AddressKind::Public,
    ))
}

fn require_len(parameters: &[u8], expected: usize, field: &'static str) -> Result<(), Error> {
    if parameters.len() == expected {
        Ok(())
    } else {
        Err(Error::with_source(
            ErrorKind::ProtocolViolation,
            PacketLengthError {
                field,
                expected,
                actual: parameters.len(),
            },
        ))
    }
}

fn require_min_len(parameters: &[u8], minimum: usize, field: &'static str) -> Result<(), Error> {
    if parameters.len() >= minimum {
        Ok(())
    } else {
        Err(Error::with_source(
            ErrorKind::ProtocolViolation,
            PacketLengthError {
                field,
                expected: minimum,
                actual: parameters.len(),
            },
        ))
    }
}

fn protocol_error(field: &'static str) -> Error {
    Error::with_source(
        ErrorKind::ProtocolViolation,
        PacketLengthError {
            field,
            expected: 1,
            actual: 0,
        },
    )
}

fn map_host_error(source: crate::classic_host::HostError) -> Error {
    let kind = match source {
        crate::classic_host::HostError::BondStore(_) => ErrorKind::InvalidBondStore,
        _ => ErrorKind::ProtocolViolation,
    };
    Error::with_source(kind, source)
}

fn prepare_explicit_identity(
    selector: UsbSelector,
    activity: ActivityNotifier,
    target: BluetoothAddress,
) -> Result<AdapterIdentityPreparation, Error> {
    if target.kind() != AddressKind::Public {
        return Err(Error::new(ErrorKind::InvalidConfiguration));
    }
    let mut target_display_order = *target.as_le_bytes();
    target_display_order.reverse();
    let mut backend = UsbIdentityBackend::new(selector, activity);
    prepare_adapter_identity(
        &mut backend,
        target_display_order,
        IdentityPreparationOptions {
            response_timeout: COMMAND_TIMEOUT,
            reenumeration_timeout: Duration::from_secs(10),
            reenumeration_poll_interval: Duration::from_millis(100),
        },
    )
    .map_err(map_identity_error)
}

fn map_identity_error(source: IdentityPreparationError) -> Error {
    let kind = match source.kind() {
        IdentityPreparationErrorKind::UnsupportedController => ErrorKind::UnsupportedController,
        IdentityPreparationErrorKind::FailedBeforeWrite => ErrorKind::OpenFailed,
        IdentityPreparationErrorKind::RecoveryRequired => {
            ErrorKind::AdapterIdentityRecoveryRequired
        }
    };
    Error::with_source(kind, source)
}

fn identity_mismatch_kind(preparation: Option<AdapterIdentityPreparation>) -> ErrorKind {
    if preparation == Some(AdapterIdentityPreparation::Rewritten) {
        ErrorKind::AdapterIdentityRecoveryRequired
    } else {
        ErrorKind::IdentityMismatch
    }
}

#[derive(Debug)]
struct PacketLengthError {
    field: &'static str,
    expected: usize,
    actual: usize,
}

impl fmt::Display for PacketLengthError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} has length {}, expected {}",
            self.field, self.actual, self.expected
        )
    }
}

impl std::error::Error for PacketLengthError {}

#[derive(Debug)]
struct CommandFailure {
    opcode: u16,
    status: u8,
}

impl fmt::Display for CommandFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "HCI command 0x{:04X} failed with status 0x{:02X}",
            self.opcode, self.status
        )
    }
}

impl std::error::Error for CommandFailure {}

impl Session {
    /// Opens and synchronously initializes one USB Bluetooth controller.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::OpenFailed`] for selector, USB, timeout, or HCI
    /// command failures. Capability, controller identity, and explicit local
    /// identity failures use their corresponding [`ErrorKind`] variants.
    pub fn open(options: OpenOptions, bonds: Box<dyn BondStore>) -> Result<Self, Error> {
        let (selector, config, local_identity, activity) = options.into_parts();
        let selector = UsbSelector::parse(selector.as_str())
            .map_err(|source| Error::with_source(ErrorKind::OpenFailed, source))?;
        let identity_preparation = match local_identity {
            LocalIdentity::AdapterDefault => None,
            LocalIdentity::Explicit(target) => Some(prepare_explicit_identity(
                selector.clone(),
                activity.clone(),
                target,
            )?),
        };
        let opened = crate::usb::open(&selector, activity)
            .map_err(|source| Error::with_source(ErrorKind::OpenFailed, source))?;
        let driver = BackendSession::initialize(UsbHciIo { opened }, config, bonds)?;
        if let LocalIdentity::Explicit(expected) = local_identity {
            if driver.capabilities.local_address() != expected {
                return Err(Error::new(identity_mismatch_kind(identity_preparation)));
            }
        }
        Ok(Self {
            driver: Box::new(driver),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, VecDeque};
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::hci::{AclAssembler, AclPacket};
    use crate::l2cap::classic::{ChannelManager, ClassicChannelState};
    use crate::sdp::{DataElement, SdpPdu};
    use crate::{BluetoothUuid, BondStoreError, ClassicBond, HidSdpPolicy, HidServiceConfig};

    const PEER: BluetoothAddress =
        BluetoothAddress::from_le_bytes([6, 5, 4, 3, 2, 1], AddressKind::Public);

    #[test]
    fn scripted_initialization_uses_exact_classic_command_sequence() {
        let (io, commands) = ScriptedIo::initialization();
        let session = BackendSession::initialize(io, config(), MemoryBondStore::default()).unwrap();

        assert_eq!(session.capabilities.local_address(), local_address());
        assert_eq!(
            session.capabilities.controller_version().company_identifier,
            10
        );
        assert_eq!(
            opcodes(&commands),
            [
                HCI_RESET,
                HCI_READ_LOCAL_SUPPORTED_COMMANDS,
                HCI_READ_LOCAL_VERSION_INFORMATION,
                HCI_READ_LOCAL_EXTENDED_FEATURES,
                HCI_SET_EVENT_MASK,
                HCI_LE_SET_EVENT_MASK,
                HCI_READ_BUFFER_SIZE,
                HCI_READ_BD_ADDR,
                HCI_WRITE_LOCAL_NAME,
                HCI_WRITE_CLASS_OF_DEVICE,
                HCI_WRITE_SIMPLE_PAIRING_MODE,
                HCI_WRITE_EXTENDED_INQUIRY_RESPONSE,
                HCI_WRITE_DEFAULT_LINK_POLICY_SETTINGS,
                HCI_WRITE_SCAN_ENABLE,
            ]
        );
        let commands = commands.lock().unwrap();
        assert_eq!(commands[8].parameters.len(), 248);
        assert_eq!(&commands[9].parameters, &[0x08, 0x25, 0x00]);
        assert_eq!(
            &commands[11].parameters[1..],
            config().extended_inquiry_response()
        );
    }

    #[test]
    fn missing_classic_capability_and_failed_command_are_typed_open_errors() {
        let (mut io, _) = ScriptedIo::initialization();
        io.responses[3] = Ok(Some(command_complete(
            HCI_READ_LOCAL_EXTENDED_FEATURES,
            [
                &[0, 0][..],
                &[0, 0, 0, 0, BR_EDR_NOT_SUPPORTED_MASK, 0, 0, 0],
            ]
            .concat(),
        )));
        let error = BackendSession::initialize(io, config(), MemoryBondStore::default())
            .err()
            .unwrap();
        assert_eq!(error.kind(), ErrorKind::UnsupportedController);

        let (mut io, _) = ScriptedIo::initialization();
        io.responses[0] = Ok(Some(command_complete_with_status(HCI_RESET, 0x0C, [])));
        let error = BackendSession::initialize(io, config(), MemoryBondStore::default())
            .err()
            .unwrap();
        assert_eq!(error.kind(), ErrorKind::OpenFailed);
    }

    #[test]
    fn pairing_latches_one_peer_and_converts_connection_events() {
        let (io, commands) = ScriptedIo::initialization();
        let responses = io.live_responses.clone();
        let mut session =
            BackendSession::initialize(io, config(), MemoryBondStore::default()).unwrap();
        session.start_pairing().unwrap();
        assert_eq!(
            &opcodes(&commands)[14..],
            [
                HCI_WRITE_SCAN_ENABLE,
                HCI_WRITE_EXTENDED_INQUIRY_RESPONSE,
                HCI_WRITE_SCAN_ENABLE,
            ]
        );

        responses.lock().unwrap().extend([
            Packet::Event(event(
                EVENT_CONNECTION_REQUEST,
                [PEER.as_le_bytes().as_slice(), &[0, 0, 0, 1]].concat(),
            )),
            Packet::Event(event(
                EVENT_CONNECTION_COMPLETE,
                [&[0, 0x40, 0], PEER.as_le_bytes().as_slice(), &[1, 0]].concat(),
            )),
        ]);
        let events = session.poll(Duration::ZERO).unwrap();
        assert_eq!(events, [Event::Connected { peer: PEER }]);
        assert!(opcodes(&commands).contains(&0x0409));
        assert!(opcodes(&commands).contains(&0x0411));
    }

    #[test]
    fn reconnect_requires_one_bond_and_uses_stored_link_key() {
        let (io, commands) = ScriptedIo::initialization();
        let responses = io.live_responses.clone();
        let mut bonds = MemoryBondStore::default();
        bonds
            .bonds
            .insert(PEER, ClassicBond::new([0xA5; 16], 4, true));
        let mut session = BackendSession::initialize(io, config(), bonds).unwrap();

        session.start_reconnect().unwrap();
        assert_eq!(opcodes(&commands).last(), Some(&HCI_CREATE_CONNECTION));
        responses.lock().unwrap().push_back(Packet::Event(event(
            EVENT_LINK_KEY_REQUEST,
            PEER.as_le_bytes().to_vec(),
        )));
        session.poll(Duration::ZERO).unwrap();

        let commands = commands.lock().unwrap();
        let reply = commands
            .iter()
            .find(|command| command.opcode == 0x040B)
            .unwrap();
        assert_eq!(&reply.parameters[..6], PEER.as_le_bytes());
        assert_eq!(&reply.parameters[6..], &[0xA5; 16]);
    }

    #[test]
    fn pair_sdp_continuation_hid_output_and_interrupt_input_share_one_session() {
        let (io, _commands) = ScriptedIo::initialization();
        let responses = io.live_responses.clone();
        let host_acl = io.acl_packets.clone();
        let mut session =
            BackendSession::initialize(io, config(), MemoryBondStore::default()).unwrap();
        session.start_pairing().unwrap();
        responses.lock().unwrap().extend([
            Packet::Event(event(
                EVENT_CONNECTION_REQUEST,
                [PEER.as_le_bytes().as_slice(), &[0, 0, 0, 1]].concat(),
            )),
            Packet::Event(event(
                EVENT_CONNECTION_COMPLETE,
                [&[0, 0x40, 0], PEER.as_le_bytes().as_slice(), &[1, 0]].concat(),
            )),
            Packet::Event(event(EVENT_AUTHENTICATION_COMPLETE, vec![0, 0x40, 0])),
            Packet::Event(event(EVENT_ENCRYPTION_CHANGE, vec![0, 0x40, 0, 1])),
        ]);
        assert_eq!(
            session.poll(Duration::ZERO).unwrap(),
            [Event::Connected { peer: PEER }]
        );

        let mut peer = TestPeer::new(0x0040, responses, host_acl);
        let (sdp_cid, events) = peer.open_channel(&mut session, SDP_PSM, 48);
        assert!(events.is_empty(), "SDP channels stay private");

        let mut continuation_state = vec![0];
        let mut attribute_lists = Vec::new();
        let mut rounds = 0;
        loop {
            rounds += 1;
            let request = SdpPdu::ServiceSearchAttributeRequest {
                transaction_id: rounds,
                service_search_pattern: DataElement::sequence([DataElement::uuid(
                    BluetoothUuid::from_u16(0x1124),
                )]),
                maximum_attribute_byte_count: 19,
                attribute_id_list: DataElement::sequence([DataElement::unsigned_integer_32(
                    0x0000_FFFF,
                )]),
                continuation_state,
            }
            .to_bytes()
            .unwrap();
            peer.send(&mut session, sdp_cid, &request);
            let response = SdpPdu::from_bytes(&peer.take_sdu(sdp_cid)).unwrap();
            let SdpPdu::ServiceSearchAttributeResponse {
                attribute_lists: chunk,
                continuation_state: next,
                ..
            } = response
            else {
                panic!("expected HID service search attribute response")
            };
            attribute_lists.extend(chunk);
            continuation_state = next;
            if continuation_state == [0] {
                break;
            }
        }
        assert!(rounds > 1, "48-byte peer MTU must exercise continuation");
        assert!(contains_bytes(&attribute_lists, b"Wireless Gamepad"));
        assert!(contains_bytes(&attribute_lists, &[0x05, 0x01]));

        let (control_cid, control_events) = peer.open_channel(&mut session, HID_CONTROL_PSM, 64);
        assert_eq!(
            control_events,
            [Event::ChannelOpened {
                channel: Channel::Control,
            }]
        );
        let (interrupt_cid, interrupt_events) =
            peer.open_channel(&mut session, HID_INTERRUPT_PSM, 64);
        assert_eq!(
            interrupt_events,
            [Event::ChannelOpened {
                channel: Channel::Interrupt,
            }]
        );

        let output_events = peer.send(&mut session, interrupt_cid, &[0xA2, 0x01, 0x00, 0x03]);
        assert_eq!(
            output_events,
            [Event::HidOutput {
                channel: Channel::Interrupt,
                payload: Box::from([0x01, 0x00, 0x03]),
            }]
        );

        assert!(session.interrupt_send_capacity_available());
        session.send_interrupt(&[0x30, 0x01]).unwrap();
        peer.pump(&mut session);
        assert_eq!(peer.take_sdu(interrupt_cid), [0xA1, 0x30, 0x01]);
        assert!(session.drain_interrupt(Duration::ZERO).is_ok());

        // The control channel remains independently usable after interrupt I/O.
        let control_events = peer.send(&mut session, control_cid, &[0xA2, 0x02]);
        assert_eq!(
            control_events,
            [Event::HidOutput {
                channel: Channel::Control,
                payload: Box::from([0x02]),
            }]
        );
    }

    #[test]
    fn active_reconnect_opens_control_then_interrupt_channels() {
        let (io, _commands) = ScriptedIo::initialization();
        let responses = io.live_responses.clone();
        let host_acl = io.acl_packets.clone();
        let mut bonds = MemoryBondStore::default();
        bonds
            .bonds
            .insert(PEER, ClassicBond::new([0xB6; 16], 4, true));
        let mut session = BackendSession::initialize(io, config(), bonds).unwrap();
        session.start_reconnect().unwrap();
        responses.lock().unwrap().extend([
            Packet::Event(event(
                EVENT_CONNECTION_COMPLETE,
                [&[0, 0x40, 0], PEER.as_le_bytes().as_slice(), &[1, 0]].concat(),
            )),
            Packet::Event(event(EVENT_ENCRYPTION_CHANGE, vec![0, 0x40, 0, 1])),
        ]);
        assert_eq!(
            session.poll(Duration::ZERO).unwrap(),
            [Event::Connected { peer: PEER }]
        );

        let mut peer = TestPeer::new(0x0040, responses, host_acl);
        peer.channels
            .register_server(
                Some(HID_CONTROL_PSM),
                ClassicChannelSpec {
                    mtu: CLASSIC_SERVER_MTU,
                },
            )
            .unwrap();
        peer.channels
            .register_server(
                Some(HID_INTERRUPT_PSM),
                ClassicChannelSpec {
                    mtu: CLASSIC_SERVER_MTU,
                },
            )
            .unwrap();
        let events = peer.pump(&mut session);

        assert_eq!(
            events,
            [
                Event::ChannelOpened {
                    channel: Channel::Control,
                },
                Event::ChannelOpened {
                    channel: Channel::Interrupt,
                },
            ],
            "control={:?} interrupt={:?}",
            session.protocols.control,
            session.protocols.interrupt,
        );
        assert!(session.interrupt_send_capacity_available());
    }

    #[test]
    fn event_queue_overflow_is_immediately_terminal() {
        let (io, _) = ScriptedIo::initialization();
        let mut session =
            BackendSession::initialize(io, config(), MemoryBondStore::default()).unwrap();

        for _ in 0..=EVENT_QUEUE_CAPACITY {
            session.enqueue_event(Event::Disconnected { reason: None });
        }

        let first = session.poll(Duration::ZERO).unwrap_err();
        assert_eq!(first.kind(), ErrorKind::EventQueueOverflow);
        let second = session.poll(Duration::ZERO).unwrap_err();
        assert_eq!(second.kind(), ErrorKind::EventQueueOverflow);
    }

    #[test]
    fn drain_interrupt_processes_completed_packets_until_host_queue_is_empty() {
        let (mut io, _) = ScriptedIo::initialization();
        io.responses[6] = Ok(Some(command_complete(
            HCI_READ_BUFFER_SIZE,
            vec![8, 0, 0, 64, 0, 0, 0],
        )));
        let responses = io.live_responses.clone();
        let host_acl = io.acl_packets.clone();
        let mut bonds = MemoryBondStore::default();
        bonds
            .bonds
            .insert(PEER, ClassicBond::new([0xB6; 16], 4, true));
        let mut session = BackendSession::initialize(io, config(), bonds).unwrap();
        session.start_reconnect().unwrap();
        responses.lock().unwrap().extend([
            Packet::Event(event(
                EVENT_CONNECTION_COMPLETE,
                [&[0, 0x40, 0], PEER.as_le_bytes().as_slice(), &[1, 0]].concat(),
            )),
            Packet::Event(event(EVENT_ENCRYPTION_CHANGE, vec![0, 0x40, 0, 1])),
        ]);
        assert_eq!(
            session.poll(Duration::ZERO).unwrap(),
            [Event::Connected { peer: PEER }]
        );

        let mut peer = TestPeer::new(0x0040, responses.clone(), host_acl);
        for psm in [HID_CONTROL_PSM, HID_INTERRUPT_PSM] {
            peer.channels
                .register_server(
                    Some(psm),
                    ClassicChannelSpec {
                        mtu: CLASSIC_SERVER_MTU,
                    },
                )
                .unwrap();
        }
        peer.pump(&mut session);
        session.send_interrupt(&[0x55; 600]).unwrap();
        assert!(!session.host.channel_output_is_flushed());
        responses.lock().unwrap().push_back(Packet::Event(event(
            EVENT_NUMBER_OF_COMPLETED_PACKETS,
            vec![1, 0x40, 0, 64, 0],
        )));

        session.drain_interrupt(Duration::from_secs(1)).unwrap();
        assert!(session.host.channel_output_is_flushed());
    }

    #[test]
    fn close_releases_hci_io_clears_pending_input_and_is_idempotent() {
        let (io, _) = ScriptedIo::initialization();
        let trace = Arc::new(Mutex::new(Vec::new()));
        let lifecycle_io = LifecycleIo {
            inner: io,
            trace: trace.clone(),
        };
        let mut session =
            BackendSession::initialize(lifecycle_io, config(), MemoryBondStore::default()).unwrap();
        session.enqueue_event(Event::Disconnected { reason: None });

        session.close().unwrap();

        let observed = trace.lock().unwrap().clone();
        assert_eq!(observed, ["close", "drop"]);
        assert!(session.pending_events.is_empty());
        assert_eq!(
            session.poll(Duration::ZERO).unwrap_err().kind(),
            ErrorKind::Closed
        );
        session.close().unwrap();
        let observed = trace.lock().unwrap().clone();
        assert_eq!(observed, ["close", "drop"]);
    }

    #[test]
    fn disconnect_is_idempotent_and_immediately_rejects_more_interrupt_input() {
        let (io, commands) = ScriptedIo::initialization();
        let mut session =
            BackendSession::initialize(io, config(), MemoryBondStore::default()).unwrap();
        session.connection = Some((PEER, 0x0040));
        session.window = ConnectionWindow::Connected;
        session.protocols.interrupt = Some(ProtocolChannel {
            cid: 0x0041,
            open: true,
        });
        assert!(session.interrupt_send_capacity_available());

        session.disconnect().unwrap();
        session.disconnect().unwrap();

        assert!(!session.interrupt_send_capacity_available());
        assert_eq!(
            session.send_interrupt(&[0x01]).unwrap_err().kind(),
            ErrorKind::SendRejected
        );
        let commands = commands.lock().unwrap();
        let disconnects = commands
            .iter()
            .filter(|command| command.opcode == HCI_DISCONNECT)
            .collect::<Vec<_>>();
        assert_eq!(disconnects.len(), 1);
        assert_eq!(
            disconnects[0].parameters,
            [0x40, 0, REMOTE_USER_TERMINATED_CONNECTION]
        );
    }

    #[test]
    fn rejected_connection_completion_does_not_poison_the_session() {
        let (io, commands) = ScriptedIo::initialization();
        let responses = io.live_responses.clone();
        let mut session =
            BackendSession::initialize(io, config(), MemoryBondStore::default()).unwrap();
        responses.lock().unwrap().push_back(Packet::Event(event(
            EVENT_CONNECTION_COMPLETE,
            [&[0, 0x44, 0], PEER.as_le_bytes().as_slice(), &[1, 0]].concat(),
        )));

        assert!(session.poll(Duration::ZERO).unwrap().is_empty());
        assert!(
            commands
                .lock()
                .unwrap()
                .iter()
                .any(|command| command.opcode == HCI_DISCONNECT
                    && command.parameters == [0x44, 0, AUTHENTICATION_FAILURE])
        );
        responses.lock().unwrap().push_back(Packet::Event(event(
            EVENT_DISCONNECTION_COMPLETE,
            vec![0, 0x44, 0, REMOTE_USER_TERMINATED_CONNECTION],
        )));

        assert!(session.poll(Duration::ZERO).unwrap().is_empty());
        session.start_pairing().unwrap();
    }

    #[test]
    fn rewritten_identity_mismatch_requires_recovery() {
        assert_eq!(
            identity_mismatch_kind(Some(AdapterIdentityPreparation::Rewritten)),
            ErrorKind::AdapterIdentityRecoveryRequired
        );
        assert_eq!(
            identity_mismatch_kind(Some(AdapterIdentityPreparation::AlreadyActive)),
            ErrorKind::IdentityMismatch
        );
    }

    struct TestPeer {
        connection_handle: u16,
        channels: ChannelManager,
        assembler: AclAssembler,
        responses: Arc<Mutex<VecDeque<Packet>>>,
        host_acl: Arc<Mutex<VecDeque<AclPacket>>>,
    }

    impl TestPeer {
        fn new(
            connection_handle: u16,
            responses: Arc<Mutex<VecDeque<Packet>>>,
            host_acl: Arc<Mutex<VecDeque<AclPacket>>>,
        ) -> Self {
            Self {
                connection_handle,
                channels: ChannelManager::new(),
                assembler: AclAssembler::default(),
                responses,
                host_acl,
            }
        }

        fn open_channel(
            &mut self,
            session: &mut BackendSession<ScriptedIo, MemoryBondStore>,
            psm: u32,
            mtu: u16,
        ) -> (u16, Vec<Event>) {
            let cid = self
                .channels
                .connect(psm, ClassicChannelSpec { mtu })
                .unwrap();
            let mut events = Vec::new();
            for _ in 0..32 {
                events.extend(self.pump(session));
                if self
                    .channels
                    .channel(cid)
                    .is_some_and(|channel| channel.state == ClassicChannelState::Open)
                {
                    return (cid, events);
                }
            }
            panic!("peer channel did not open for PSM {psm:#06x}");
        }

        fn send(
            &mut self,
            session: &mut BackendSession<ScriptedIo, MemoryBondStore>,
            cid: u16,
            sdu: &[u8],
        ) -> Vec<Event> {
            self.channels.send(cid, sdu).unwrap();
            self.pump(session)
        }

        fn take_sdu(&mut self, cid: u16) -> Vec<u8> {
            self.channels
                .channel_mut(cid)
                .and_then(|channel| channel.pop_received())
                .expect("peer channel received one SDU")
        }

        fn pump(
            &mut self,
            session: &mut BackendSession<ScriptedIo, MemoryBondStore>,
        ) -> Vec<Event> {
            let mut events = Vec::new();
            for _ in 0..64 {
                let outbound = self.channels.drain_outbound();
                let mut progress = !outbound.is_empty();
                if progress {
                    self.responses
                        .lock()
                        .unwrap()
                        .extend(outbound.into_iter().map(|pdu| {
                            Packet::Acl(
                                AclPacket::new(self.connection_handle, 0, 0, pdu.to_bytes(false))
                                    .unwrap(),
                            )
                        }));
                }

                let polled = session.poll(Duration::ZERO).unwrap();
                progress |= !polled.is_empty();
                events.extend(polled);

                let host_packets = {
                    let mut packets = self.host_acl.lock().unwrap();
                    std::mem::take(&mut *packets)
                };
                progress |= !host_packets.is_empty();
                for packet in host_packets {
                    if let Some(bytes) = self.assembler.feed(&packet).unwrap() {
                        self.channels.process_bytes(&bytes).unwrap();
                    }
                }
                if !progress {
                    return events;
                }
            }
            panic!("peer/session pump did not quiesce");
        }
    }

    fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }

    #[derive(Default)]
    struct MemoryBondStore {
        bonds: HashMap<BluetoothAddress, ClassicBond>,
    }

    impl BondStore for MemoryBondStore {
        fn load(&self, peer: BluetoothAddress) -> Result<Option<ClassicBond>, BondStoreError> {
            Ok(self.bonds.get(&peer).cloned())
        }

        fn load_all(&self) -> Result<Vec<(BluetoothAddress, ClassicBond)>, BondStoreError> {
            Ok(self
                .bonds
                .iter()
                .map(|(peer, bond)| (*peer, bond.clone()))
                .collect())
        }

        fn upsert(
            &mut self,
            peer: BluetoothAddress,
            bond: ClassicBond,
        ) -> Result<(), BondStoreError> {
            self.bonds.insert(peer, bond);
            Ok(())
        }
    }

    struct ScriptedIo {
        responses: Vec<Result<Option<Packet>, Error>>,
        next_response: usize,
        live_responses: Arc<Mutex<VecDeque<Packet>>>,
        commands: Arc<Mutex<Vec<CommandPacket>>>,
        acl_packets: Arc<Mutex<VecDeque<AclPacket>>>,
    }

    impl ScriptedIo {
        fn initialization() -> (Self, Arc<Mutex<Vec<CommandPacket>>>) {
            let responses = initialization_responses()
                .into_iter()
                .map(|packet| Ok(Some(packet)))
                .collect();
            let commands = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    responses,
                    next_response: 0,
                    live_responses: Arc::new(Mutex::new(VecDeque::new())),
                    commands: commands.clone(),
                    acl_packets: Arc::new(Mutex::new(VecDeque::new())),
                },
                commands,
            )
        }
    }

    impl HciIo for ScriptedIo {
        fn send(&mut self, packet: &Packet) -> Result<(), Error> {
            match packet {
                Packet::Command(command) => self.commands.lock().unwrap().push(command.clone()),
                Packet::Acl(packet) => self.acl_packets.lock().unwrap().push_back(packet.clone()),
                Packet::Event(_) => return Err(Error::new(ErrorKind::ProtocolViolation)),
            }
            Ok(())
        }

        fn recv_timeout(&mut self, _timeout: Duration) -> Result<Option<Packet>, Error> {
            if let Some(response) = self.responses.get(self.next_response) {
                self.next_response += 1;
                return response.clone();
            }
            Ok(self.live_responses.lock().unwrap().pop_front())
        }

        fn metadata(&self) -> UsbAdapterMetadata {
            UsbAdapterMetadata {
                vendor_id: 0x0A12,
                product_id: 1,
                bus: 1,
                device_address: 7,
                ports: [2, 3].into(),
            }
        }

        fn close(&mut self) -> Result<(), Error> {
            Ok(())
        }
    }

    struct LifecycleIo {
        inner: ScriptedIo,
        trace: Arc<Mutex<Vec<&'static str>>>,
    }

    impl HciIo for LifecycleIo {
        fn send(&mut self, packet: &Packet) -> Result<(), Error> {
            self.inner.send(packet)
        }

        fn recv_timeout(&mut self, timeout: Duration) -> Result<Option<Packet>, Error> {
            self.inner.recv_timeout(timeout)
        }

        fn metadata(&self) -> UsbAdapterMetadata {
            self.inner.metadata()
        }

        fn close(&mut self) -> Result<(), Error> {
            self.trace.lock().unwrap().push("close");
            Ok(())
        }
    }

    impl Drop for LifecycleIo {
        fn drop(&mut self) {
            self.trace
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push("drop");
        }
    }

    fn initialization_responses() -> Vec<Packet> {
        let mut responses = vec![
            command_complete(HCI_RESET, Vec::new()),
            command_complete(HCI_READ_LOCAL_SUPPORTED_COMMANDS, vec![0; 64]),
            command_complete(
                HCI_READ_LOCAL_VERSION_INFORMATION,
                vec![9, 0x34, 0x12, 9, 10, 0, 0x78, 0x56],
            ),
            command_complete(
                HCI_READ_LOCAL_EXTENDED_FEATURES,
                vec![0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            ),
            command_complete(HCI_SET_EVENT_MASK, Vec::new()),
            command_complete(HCI_LE_SET_EVENT_MASK, Vec::new()),
            command_complete(HCI_READ_BUFFER_SIZE, vec![0xFD, 0x03, 0, 64, 0, 0, 0]),
            command_complete(HCI_READ_BD_ADDR, local_address().as_le_bytes().to_vec()),
        ];
        responses.extend(
            [
                HCI_WRITE_LOCAL_NAME,
                HCI_WRITE_CLASS_OF_DEVICE,
                HCI_WRITE_SIMPLE_PAIRING_MODE,
                HCI_WRITE_EXTENDED_INQUIRY_RESPONSE,
                HCI_WRITE_DEFAULT_LINK_POLICY_SETTINGS,
                HCI_WRITE_SCAN_ENABLE,
            ]
            .map(|opcode| command_complete(opcode, Vec::new())),
        );
        responses
    }

    fn command_complete(opcode: u16, return_parameters: Vec<u8>) -> Packet {
        command_complete_with_status(opcode, 0, return_parameters)
    }

    fn command_complete_with_status(
        opcode: u16,
        status: u8,
        return_parameters: impl IntoIterator<Item = u8>,
    ) -> Packet {
        let mut parameters = vec![1];
        parameters.extend_from_slice(&opcode.to_le_bytes());
        parameters.push(status);
        parameters.extend(return_parameters);
        Packet::Event(event(EVENT_COMMAND_COMPLETE, parameters))
    }

    fn event(event_code: u8, parameters: Vec<u8>) -> EventPacket {
        EventPacket::new(event_code, parameters).unwrap()
    }

    fn opcodes(commands: &Arc<Mutex<Vec<CommandPacket>>>) -> Vec<u16> {
        commands
            .lock()
            .unwrap()
            .iter()
            .map(|command| command.opcode)
            .collect()
    }

    fn local_address() -> BluetoothAddress {
        BluetoothAddress::from_le_bytes([0x7D, 0x9F, 0xF9, 0xDC, 0x1B, 0], AddressKind::Public)
    }

    fn config() -> SessionConfig {
        SessionConfig::new(
            "Wireless Gamepad",
            0x0000_2508,
            HidServiceConfig::new(
                [0x05, 0x01],
                HidSdpPolicy {
                    service_name: "Wireless Gamepad".into(),
                    service_description: None,
                    provider_name: None,
                    device_release_number: None,
                    bluetooth_profile_version: 0x0101,
                    parser_version: 0x0111,
                    device_subclass: 8,
                    country_code: 0,
                    virtual_cable: true,
                    reconnect_initiate: true,
                    remote_wake: None,
                    profile_version: 0x0101,
                    supervision_timeout: 0x0C80,
                    normally_connectable: true,
                    boot_device: false,
                    ssr_host_max_latency: 0x0640,
                    ssr_host_min_timeout: 0x0320,
                },
            ),
        )
        .unwrap()
    }
}
