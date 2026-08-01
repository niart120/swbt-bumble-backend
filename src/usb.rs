// Derived from bumble-transport USB HCI support at cb55e2d. Rewritten for
// command/event/ACL only and without direct libusb1-sys or isochronous code.

use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rusb::{
    Context, Device, DeviceHandle, Direction, Recipient, RequestType, TransferType, UsbContext,
};

#[cfg(test)]
use crate::hci::{AclPacket, CommandPacket, EventPacket};
use crate::hci::{CodecError, Packet};

const USB_DEVICE_CLASS_DEVICE: u8 = 0x00;
const USB_DEVICE_CLASS_WIRELESS_CONTROLLER: u8 = 0xE0;
const USB_DEVICE_SUBCLASS_RF_CONTROLLER: u8 = 0x01;
const USB_DEVICE_PROTOCOL_BLUETOOTH_PRIMARY_CONTROLLER: u8 = 0x01;
const MAX_HCI_PACKET_SIZE: usize = 4096;
const READ_TIMEOUT: Duration = Duration::from_millis(10);
const WRITE_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum UsbSelector {
    Index(usize),
    VidPid {
        vendor_id: u16,
        product_id: u16,
        serial_number: Option<String>,
        occurrence: usize,
    },
    Path {
        bus: u8,
        ports: Vec<u8>,
    },
}

impl UsbSelector {
    pub(crate) fn parse(value: &str) -> Result<Self, UsbError> {
        let value = value.strip_prefix("usb:").unwrap_or(value);
        if value.is_empty() {
            return Err(UsbError::InvalidSelector);
        }
        if let Some((vendor, product)) = value.split_once(':') {
            let vendor_id = parse_hex_id(vendor)?;
            let (product, serial_number, occurrence) =
                if let Some((product, serial)) = product.split_once('/') {
                    if serial.is_empty() {
                        return Err(UsbError::InvalidSelector);
                    }
                    (product, Some(serial.to_owned()), 0)
                } else if let Some((product, occurrence)) = product.split_once('#') {
                    (
                        product,
                        None,
                        occurrence
                            .parse::<usize>()
                            .map_err(|_| UsbError::InvalidSelector)?,
                    )
                } else {
                    (product, None, 0)
                };
            return Ok(Self::VidPid {
                vendor_id,
                product_id: parse_hex_id(product)?,
                serial_number,
                occurrence,
            });
        }
        if let Some((bus, ports)) = value.split_once('-') {
            let bus = bus.parse::<u8>().map_err(|_| UsbError::InvalidSelector)?;
            let ports = ports
                .split('.')
                .map(|port| port.parse::<u8>().map_err(|_| UsbError::InvalidSelector))
                .collect::<Result<Vec<_>, _>>()?;
            if ports.is_empty() {
                return Err(UsbError::InvalidSelector);
            }
            return Ok(Self::Path { bus, ports });
        }
        value
            .parse::<usize>()
            .map(Self::Index)
            .map_err(|_| UsbError::InvalidSelector)
    }
}

fn parse_hex_id(value: &str) -> Result<u16, UsbError> {
    u16::from_str_radix(value, 16).map_err(|_| UsbError::InvalidSelector)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct UsbEndpointInfo {
    address: u8,
    direction: Direction,
    transfer_type: TransferType,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UsbInterfaceInfo {
    configuration: u8,
    interface: u8,
    alternate: u8,
    class: u8,
    subclass: u8,
    protocol: u8,
    endpoints: Vec<UsbEndpointInfo>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct UsbLayout {
    configuration: u8,
    interface: u8,
    alternate: u8,
    interrupt_in: u8,
    bulk_in: u8,
    bulk_out: u8,
}

fn select_layout(interfaces: &[UsbInterfaceInfo]) -> Option<UsbLayout> {
    interfaces.iter().find_map(|interface| {
        if (interface.class, interface.subclass, interface.protocol)
            != (
                USB_DEVICE_CLASS_WIRELESS_CONTROLLER,
                USB_DEVICE_SUBCLASS_RF_CONTROLLER,
                USB_DEVICE_PROTOCOL_BLUETOOTH_PRIMARY_CONTROLLER,
            )
        {
            return None;
        }
        let endpoint = |direction, transfer_type| {
            interface
                .endpoints
                .iter()
                .find(|endpoint| {
                    endpoint.direction == direction && endpoint.transfer_type == transfer_type
                })
                .map(|endpoint| endpoint.address)
        };
        Some(UsbLayout {
            configuration: interface.configuration,
            interface: interface.interface,
            alternate: interface.alternate,
            interrupt_in: endpoint(Direction::In, TransferType::Interrupt)?,
            bulk_in: endpoint(Direction::In, TransferType::Bulk)?,
            bulk_out: endpoint(Direction::Out, TransferType::Bulk)?,
        })
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AdapterMetadata {
    vendor_id: u16,
    product_id: u16,
    bus: u8,
    address: u8,
    ports: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum UsbTransferError {
    Timeout,
    Disconnected,
    Access,
    Other(String),
}

impl fmt::Display for UsbTransferError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for UsbTransferError {}

#[derive(Debug)]
pub(crate) enum UsbError {
    InvalidSelector,
    DeviceNotFound,
    NoHciInterface,
    Transfer(UsbTransferError),
    Codec(CodecError),
    PartialWrite { expected: usize, actual: usize },
    PacketTooLarge { declared: usize, maximum: usize },
    Closed,
    Rusb(rusb::Error),
    ReaderPanicked,
}

impl fmt::Display for UsbError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for UsbError {}

impl From<UsbTransferError> for UsbError {
    fn from(error: UsbTransferError) -> Self {
        Self::Transfer(error)
    }
}

impl From<CodecError> for UsbError {
    fn from(error: CodecError) -> Self {
        Self::Codec(error)
    }
}

impl From<rusb::Error> for UsbError {
    fn from(error: rusb::Error) -> Self {
        Self::Rusb(error)
    }
}

pub(crate) trait UsbIo: Send {
    fn read_interrupt(
        &mut self,
        endpoint: u8,
        buffer: &mut [u8],
        timeout: Duration,
    ) -> Result<usize, UsbTransferError>;

    fn read_bulk(
        &mut self,
        endpoint: u8,
        buffer: &mut [u8],
        timeout: Duration,
    ) -> Result<usize, UsbTransferError>;

    fn write_control(
        &mut self,
        request_type: u8,
        buffer: &[u8],
        timeout: Duration,
    ) -> Result<usize, UsbTransferError>;

    fn write_bulk(
        &mut self,
        endpoint: u8,
        buffer: &[u8],
        timeout: Duration,
    ) -> Result<usize, UsbTransferError>;
}

struct UsbTransport<B> {
    backend: B,
    layout: UsbLayout,
    metadata: AdapterMetadata,
    next_endpoint: bool,
    event_bytes: Vec<u8>,
    acl_bytes: Vec<u8>,
    pending: VecDeque<Packet>,
    shutdown: Arc<AtomicBool>,
}

impl<B> UsbTransport<B> {
    fn new(backend: B, layout: UsbLayout, metadata: AdapterMetadata) -> Self {
        Self {
            backend,
            layout,
            metadata,
            next_endpoint: false,
            event_bytes: Vec::new(),
            acl_bytes: Vec::new(),
            pending: VecDeque::new(),
            shutdown: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl<B: UsbIo> UsbTransport<B> {
    fn poll_packet(&mut self) -> Result<Option<Packet>, UsbError> {
        if self.shutdown.load(Ordering::Acquire) {
            return Ok(None);
        }
        if let Some(packet) = self.pending.pop_front() {
            return Ok(Some(packet));
        }
        let events = !self.next_endpoint;
        self.next_endpoint = events;
        let mut transfer = vec![0_u8; MAX_HCI_PACKET_SIZE - 1];
        let result = if events {
            self.backend
                .read_interrupt(self.layout.interrupt_in, &mut transfer, READ_TIMEOUT)
        } else {
            self.backend
                .read_bulk(self.layout.bulk_in, &mut transfer, READ_TIMEOUT)
        };
        let count = match result {
            Ok(count) => count,
            Err(UsbTransferError::Timeout) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if count == 0 {
            return Ok(None);
        }
        if count > transfer.len() {
            return Err(UsbError::Transfer(UsbTransferError::Other(
                "USB backend returned a count larger than its buffer".into(),
            )));
        }
        let (packet_type, buffered) = if events {
            (0x04, &mut self.event_bytes)
        } else {
            (0x02, &mut self.acl_bytes)
        };
        buffered.extend_from_slice(&transfer[..count]);
        self.pending.extend(frame_packets(packet_type, buffered)?);
        Ok(self.pending.pop_front())
    }

    fn write_packet(&mut self, packet: &Packet) -> Result<(), UsbError> {
        if self.shutdown.load(Ordering::Acquire) {
            return Err(UsbError::Closed);
        }
        let bytes = packet.to_bytes();
        let (packet_type, payload) = bytes.split_first().ok_or(CodecError::EmptyPacket)?;
        let actual = match packet_type {
            0x01 => self.backend.write_control(
                rusb::request_type(Direction::Out, RequestType::Class, Recipient::Device),
                payload,
                WRITE_TIMEOUT,
            ),
            0x02 => self
                .backend
                .write_bulk(self.layout.bulk_out, payload, WRITE_TIMEOUT),
            other => {
                return Err(UsbError::Codec(CodecError::UnsupportedPacketType(*other)));
            }
        }?;
        if actual != payload.len() {
            return Err(UsbError::PartialWrite {
                expected: payload.len(),
                actual,
            });
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) enum ReaderEvent {
    Packet(Packet),
    Failed(UsbError),
    Ended,
}

pub(crate) struct PacketReader {
    receiver: Receiver<ReaderEvent>,
    shutdown: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl PacketReader {
    fn spawn<B: UsbIo + 'static>(mut transport: UsbTransport<B>) -> Self {
        let shutdown = transport.shutdown.clone();
        let worker_shutdown = shutdown.clone();
        let (sender, receiver) = mpsc::channel();
        let join = thread::spawn(move || {
            while !worker_shutdown.load(Ordering::Acquire) {
                match transport.poll_packet() {
                    Ok(Some(packet)) => {
                        if sender.send(ReaderEvent::Packet(packet)).is_err() {
                            break;
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        let _ = sender.send(ReaderEvent::Failed(error));
                        break;
                    }
                }
            }
            let _ = sender.send(ReaderEvent::Ended);
        });
        Self {
            receiver,
            shutdown,
            join: Some(join),
        }
    }

    pub(crate) fn recv_timeout(&self, timeout: Duration) -> Result<ReaderEvent, RecvTimeoutError> {
        self.receiver.recv_timeout(timeout)
    }

    pub(crate) fn close(&mut self) -> Result<(), UsbError> {
        self.shutdown.store(true, Ordering::Release);
        if let Some(join) = self.join.take() {
            join.join().map_err(|_| UsbError::ReaderPanicked)?;
        }
        Ok(())
    }
}

impl Drop for PacketReader {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

pub(crate) struct PacketSink<B> {
    transport: UsbTransport<B>,
}

impl<B: UsbIo> PacketSink<B> {
    pub(crate) fn send(&mut self, packet: &Packet) -> Result<(), UsbError> {
        self.transport.write_packet(packet)
    }

    pub(crate) fn metadata(&self) -> &AdapterMetadata {
        &self.transport.metadata
    }
}

fn split_transport<B: UsbIo + Clone + 'static>(
    transport: UsbTransport<B>,
) -> (PacketReader, PacketSink<B>) {
    let source = UsbTransport {
        backend: transport.backend.clone(),
        layout: transport.layout,
        metadata: transport.metadata.clone(),
        next_endpoint: false,
        event_bytes: Vec::new(),
        acl_bytes: Vec::new(),
        pending: VecDeque::new(),
        shutdown: transport.shutdown.clone(),
    };
    (PacketReader::spawn(source), PacketSink { transport })
}

fn frame_packets(packet_type: u8, buffered: &mut Vec<u8>) -> Result<Vec<Packet>, UsbError> {
    let mut packets = Vec::new();
    loop {
        let raw_length = match packet_type {
            0x04 if buffered.len() >= 2 => 2 + usize::from(buffered[1]),
            0x02 if buffered.len() >= 4 => {
                4 + usize::from(u16::from_le_bytes([buffered[2], buffered[3]]))
            }
            0x04 | 0x02 => break,
            other => return Err(UsbError::Codec(CodecError::UnsupportedPacketType(other))),
        };
        let declared = raw_length + 1;
        if declared > MAX_HCI_PACKET_SIZE {
            buffered.clear();
            return Err(UsbError::PacketTooLarge {
                declared,
                maximum: MAX_HCI_PACKET_SIZE,
            });
        }
        if buffered.len() < raw_length {
            break;
        }
        let mut framed = Vec::with_capacity(declared);
        framed.push(packet_type);
        framed.extend(buffered.drain(..raw_length));
        packets.push(Packet::from_bytes(&framed)?);
    }
    Ok(packets)
}

#[derive(Clone)]
struct RusbIo {
    handle: Arc<DeviceHandle<Context>>,
}

impl UsbIo for RusbIo {
    fn read_interrupt(
        &mut self,
        endpoint: u8,
        buffer: &mut [u8],
        timeout: Duration,
    ) -> Result<usize, UsbTransferError> {
        self.handle
            .read_interrupt(endpoint, buffer, timeout)
            .map_err(map_rusb_error)
    }

    fn read_bulk(
        &mut self,
        endpoint: u8,
        buffer: &mut [u8],
        timeout: Duration,
    ) -> Result<usize, UsbTransferError> {
        self.handle
            .read_bulk(endpoint, buffer, timeout)
            .map_err(map_rusb_error)
    }

    fn write_control(
        &mut self,
        request_type: u8,
        buffer: &[u8],
        timeout: Duration,
    ) -> Result<usize, UsbTransferError> {
        self.handle
            .write_control(request_type, 0, 0, 0, buffer, timeout)
            .map_err(map_rusb_error)
    }

    fn write_bulk(
        &mut self,
        endpoint: u8,
        buffer: &[u8],
        timeout: Duration,
    ) -> Result<usize, UsbTransferError> {
        self.handle
            .write_bulk(endpoint, buffer, timeout)
            .map_err(map_rusb_error)
    }
}

fn map_rusb_error(error: rusb::Error) -> UsbTransferError {
    match error {
        rusb::Error::Timeout => UsbTransferError::Timeout,
        rusb::Error::NoDevice => UsbTransferError::Disconnected,
        rusb::Error::Access => UsbTransferError::Access,
        error => UsbTransferError::Other(error.to_string()),
    }
}

fn open_transport(selector: &UsbSelector) -> Result<UsbTransport<RusbIo>, UsbError> {
    let context = Context::new()?;
    let devices = context.devices()?;
    let device = select_device(&devices, selector)?;
    let descriptor = device.device_descriptor()?;
    let layout = select_layout(&interface_infos(&device)?).ok_or(UsbError::NoHciInterface)?;
    let handle = device.open()?;
    match handle.set_auto_detach_kernel_driver(true) {
        Ok(()) | Err(rusb::Error::NotSupported) => {}
        Err(error) => return Err(error.into()),
    }
    if handle.active_configuration().ok() != Some(layout.configuration) {
        handle.set_active_configuration(layout.configuration)?;
    }
    handle.claim_interface(layout.interface)?;
    if layout.alternate != 0 {
        handle.set_alternate_setting(layout.interface, layout.alternate)?;
    }
    let metadata = AdapterMetadata {
        vendor_id: descriptor.vendor_id(),
        product_id: descriptor.product_id(),
        bus: device.bus_number(),
        address: device.address(),
        ports: device.port_numbers().unwrap_or_default(),
    };
    Ok(UsbTransport::new(
        RusbIo {
            handle: Arc::new(handle),
        },
        layout,
        metadata,
    ))
}

fn select_device<T: UsbContext>(
    devices: &rusb::DeviceList<T>,
    selector: &UsbSelector,
) -> Result<Device<T>, UsbError> {
    match selector {
        UsbSelector::Index(index) => devices
            .iter()
            .filter(|device| device_is_bluetooth_hci(device).unwrap_or(false))
            .nth(*index)
            .ok_or(UsbError::DeviceNotFound),
        UsbSelector::Path { bus, ports } => devices
            .iter()
            .find(|device| {
                device.bus_number() == *bus
                    && device.port_numbers().ok().as_deref() == Some(ports.as_slice())
            })
            .ok_or(UsbError::DeviceNotFound),
        UsbSelector::VidPid {
            vendor_id,
            product_id,
            serial_number,
            occurrence,
        } => {
            let mut left = *occurrence;
            for device in devices.iter() {
                let Ok(descriptor) = device.device_descriptor() else {
                    continue;
                };
                if descriptor.vendor_id() != *vendor_id || descriptor.product_id() != *product_id {
                    continue;
                }
                if let Some(expected) = serial_number {
                    let handle = device.open()?;
                    if handle.read_serial_number_string_ascii(&descriptor)? != *expected {
                        continue;
                    }
                }
                if left == 0 {
                    return Ok(device);
                }
                left -= 1;
            }
            Err(UsbError::DeviceNotFound)
        }
    }
}

fn device_is_bluetooth_hci<T: UsbContext>(device: &Device<T>) -> Result<bool, UsbError> {
    let descriptor = device.device_descriptor()?;
    if (
        descriptor.class_code(),
        descriptor.sub_class_code(),
        descriptor.protocol_code(),
    ) == (
        USB_DEVICE_CLASS_WIRELESS_CONTROLLER,
        USB_DEVICE_SUBCLASS_RF_CONTROLLER,
        USB_DEVICE_PROTOCOL_BLUETOOTH_PRIMARY_CONTROLLER,
    ) {
        return Ok(true);
    }
    if descriptor.class_code() != USB_DEVICE_CLASS_DEVICE {
        return Ok(false);
    }
    Ok(interface_infos(device)?.iter().any(|interface| {
        (interface.class, interface.subclass, interface.protocol)
            == (
                USB_DEVICE_CLASS_WIRELESS_CONTROLLER,
                USB_DEVICE_SUBCLASS_RF_CONTROLLER,
                USB_DEVICE_PROTOCOL_BLUETOOTH_PRIMARY_CONTROLLER,
            )
    }))
}

fn interface_infos<T: UsbContext>(device: &Device<T>) -> Result<Vec<UsbInterfaceInfo>, UsbError> {
    let descriptor = device.device_descriptor()?;
    let mut infos = Vec::new();
    for config_index in 0..descriptor.num_configurations() {
        let config = device.config_descriptor(config_index)?;
        for interface in config.interfaces() {
            for setting in interface.descriptors() {
                infos.push(UsbInterfaceInfo {
                    configuration: config.number(),
                    interface: setting.interface_number(),
                    alternate: setting.setting_number(),
                    class: setting.class_code(),
                    subclass: setting.sub_class_code(),
                    protocol: setting.protocol_code(),
                    endpoints: setting
                        .endpoint_descriptors()
                        .map(|endpoint| UsbEndpointInfo {
                            address: endpoint.address(),
                            direction: endpoint.direction(),
                            transfer_type: endpoint.transfer_type(),
                        })
                        .collect(),
                });
            }
        }
    }
    Ok(infos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct ScriptState {
        events: VecDeque<Result<Vec<u8>, UsbTransferError>>,
        acl: VecDeque<Result<Vec<u8>, UsbTransferError>>,
        control_writes: Vec<Vec<u8>>,
        bulk_writes: Vec<Vec<u8>>,
    }

    #[derive(Clone, Default)]
    struct ScriptedIo {
        state: Arc<Mutex<ScriptState>>,
    }

    impl UsbIo for ScriptedIo {
        fn read_interrupt(
            &mut self,
            _endpoint: u8,
            buffer: &mut [u8],
            _timeout: Duration,
        ) -> Result<usize, UsbTransferError> {
            read_script(&self.state, true, buffer)
        }

        fn read_bulk(
            &mut self,
            _endpoint: u8,
            buffer: &mut [u8],
            _timeout: Duration,
        ) -> Result<usize, UsbTransferError> {
            read_script(&self.state, false, buffer)
        }

        fn write_control(
            &mut self,
            _request_type: u8,
            buffer: &[u8],
            _timeout: Duration,
        ) -> Result<usize, UsbTransferError> {
            self.state
                .lock()
                .unwrap()
                .control_writes
                .push(buffer.to_vec());
            Ok(buffer.len())
        }

        fn write_bulk(
            &mut self,
            _endpoint: u8,
            buffer: &[u8],
            _timeout: Duration,
        ) -> Result<usize, UsbTransferError> {
            self.state.lock().unwrap().bulk_writes.push(buffer.to_vec());
            Ok(buffer.len())
        }
    }

    fn read_script(
        state: &Mutex<ScriptState>,
        events: bool,
        buffer: &mut [u8],
    ) -> Result<usize, UsbTransferError> {
        let mut state = state.lock().unwrap();
        let queue = if events {
            &mut state.events
        } else {
            &mut state.acl
        };
        let bytes = queue
            .pop_front()
            .unwrap_or(Err(UsbTransferError::Timeout))?;
        buffer[..bytes.len()].copy_from_slice(&bytes);
        Ok(bytes.len())
    }

    fn layout() -> UsbLayout {
        UsbLayout {
            configuration: 1,
            interface: 0,
            alternate: 0,
            interrupt_in: 0x81,
            bulk_in: 0x82,
            bulk_out: 0x02,
        }
    }

    fn metadata() -> AdapterMetadata {
        AdapterMetadata {
            vendor_id: 0x0A12,
            product_id: 0x0001,
            bus: 1,
            address: 2,
            ports: vec![3, 4],
        }
    }

    #[test]
    fn selectors_cover_index_vid_pid_serial_occurrence_and_path() {
        assert_eq!(UsbSelector::parse("usb:0").unwrap(), UsbSelector::Index(0));
        assert_eq!(
            UsbSelector::parse("0a12:0001/CSR").unwrap(),
            UsbSelector::VidPid {
                vendor_id: 0x0A12,
                product_id: 0x0001,
                serial_number: Some("CSR".into()),
                occurrence: 0,
            }
        );
        assert_eq!(
            UsbSelector::parse("0a12:0001#2").unwrap(),
            UsbSelector::VidPid {
                vendor_id: 0x0A12,
                product_id: 0x0001,
                serial_number: None,
                occurrence: 2,
            }
        );
        assert_eq!(
            UsbSelector::parse("1-3.4").unwrap(),
            UsbSelector::Path {
                bus: 1,
                ports: vec![3, 4],
            }
        );
    }

    #[test]
    fn endpoint_selection_requires_bluetooth_interrupt_and_bulk_triplet() {
        let compatible = UsbInterfaceInfo {
            configuration: 1,
            interface: 0,
            alternate: 0,
            class: 0xE0,
            subclass: 1,
            protocol: 1,
            endpoints: vec![
                UsbEndpointInfo {
                    address: 0x81,
                    direction: Direction::In,
                    transfer_type: TransferType::Interrupt,
                },
                UsbEndpointInfo {
                    address: 0x82,
                    direction: Direction::In,
                    transfer_type: TransferType::Bulk,
                },
                UsbEndpointInfo {
                    address: 0x02,
                    direction: Direction::Out,
                    transfer_type: TransferType::Bulk,
                },
            ],
        };

        assert_eq!(select_layout(&[compatible]), Some(layout()));
    }

    #[test]
    fn scripted_transport_reads_event_and_acl_and_writes_command_and_acl() {
        let backend = ScriptedIo::default();
        {
            let mut state = backend.state.lock().unwrap();
            state
                .events
                .push_back(Ok(vec![0x0E, 0x04, 0x01, 0x03, 0x0C, 0x00]));
            state
                .acl
                .push_back(Ok(vec![0x40, 0x20, 0x02, 0x00, 0xAA, 0xBB]));
        }
        let mut transport = UsbTransport::new(backend.clone(), layout(), metadata());

        assert!(matches!(
            transport.poll_packet().unwrap(),
            Some(Packet::Event(_))
        ));
        assert!(matches!(
            transport.poll_packet().unwrap(),
            Some(Packet::Acl(_))
        ));
        transport
            .write_packet(&Packet::Command(
                CommandPacket::new(0x0C03, Vec::new()).unwrap(),
            ))
            .unwrap();
        transport
            .write_packet(&Packet::Acl(
                AclPacket::new(0x0040, 2, 0, vec![0xCC]).unwrap(),
            ))
            .unwrap();

        let state = backend.state.lock().unwrap();
        assert_eq!(state.control_writes, [vec![0x03, 0x0C, 0x00]]);
        assert_eq!(state.bulk_writes, [vec![0x40, 0x20, 0x01, 0x00, 0xCC]]);
    }

    #[test]
    fn event_framer_preserves_partial_and_multiple_usb_transfers() {
        let backend = ScriptedIo::default();
        {
            let mut state = backend.state.lock().unwrap();
            state.events.push_back(Ok(vec![0x0E, 0x04, 0x01]));
            state.events.push_back(Ok(vec![
                0x03, 0x0C, 0x00, 0xFF, 0x01, 0xAA, 0xFF, 0x01, 0xBB,
            ]));
        }
        let mut transport = UsbTransport::new(backend, layout(), metadata());

        assert!(transport.poll_packet().unwrap().is_none());
        assert!(transport.poll_packet().unwrap().is_none());
        assert!(matches!(
            transport.poll_packet().unwrap(),
            Some(Packet::Event(EventPacket {
                event_code: 0x0E,
                ..
            }))
        ));
        assert!(matches!(
            transport.poll_packet().unwrap(),
            Some(Packet::Event(EventPacket {
                event_code: 0xFF,
                parameters,
            })) if parameters == [0xAA]
        ));
        assert!(matches!(
            transport.poll_packet().unwrap(),
            Some(Packet::Event(EventPacket {
                event_code: 0xFF,
                parameters,
            })) if parameters == [0xBB]
        ));
    }

    #[test]
    fn reader_close_requests_cancellation_and_joins_worker() {
        let backend = ScriptedIo::default();
        backend
            .state
            .lock()
            .unwrap()
            .events
            .push_back(Ok(vec![0x0E, 0x04, 0x01, 0x03, 0x0C, 0x00]));
        let transport = UsbTransport::new(backend, layout(), metadata());
        let (mut reader, mut sink) = split_transport(transport);

        assert_eq!(sink.metadata(), &metadata());
        assert!(matches!(
            reader.recv_timeout(Duration::from_secs(1)).unwrap(),
            ReaderEvent::Packet(Packet::Event(_))
        ));
        reader.close().unwrap();
        assert!(reader.join.is_none());
        assert!(reader.shutdown.load(Ordering::Acquire));
        assert!(matches!(
            sink.send(&Packet::Command(
                CommandPacket::new(0x0C03, Vec::new()).unwrap()
            ))
            .unwrap_err(),
            UsbError::Closed
        ));
    }

    #[test]
    fn reader_surfaces_disconnect_once_then_ends() {
        let backend = ScriptedIo::default();
        backend
            .state
            .lock()
            .unwrap()
            .events
            .push_back(Err(UsbTransferError::Disconnected));
        let transport = UsbTransport::new(backend, layout(), metadata());
        let (mut reader, _sink) = split_transport(transport);

        assert!(matches!(
            reader.recv_timeout(Duration::from_secs(1)).unwrap(),
            ReaderEvent::Failed(UsbError::Transfer(UsbTransferError::Disconnected))
        ));
        assert!(matches!(
            reader.recv_timeout(Duration::from_secs(1)).unwrap(),
            ReaderEvent::Ended
        ));
        reader.close().unwrap();
    }

    #[test]
    fn event_packets_are_not_valid_usb_output() {
        let backend = ScriptedIo::default();
        let mut transport = UsbTransport::new(backend, layout(), metadata());
        let event = Packet::Event(EventPacket::new(0xFF, vec![1]).unwrap());

        assert!(matches!(
            transport.write_packet(&event).unwrap_err(),
            UsbError::Codec(CodecError::UnsupportedPacketType(0x04))
        ));
    }
}
