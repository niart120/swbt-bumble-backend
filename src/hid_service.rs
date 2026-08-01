// Originally implemented in niart120/swbt-rs at a36a69b. Moved and modified
// for the backend-owned SDP and HIDP codecs; L2CAP channel ownership remains
// in the private session runtime. See PROVENANCE.md.

use std::fmt;

use crate::hidp::{
    DeviceDelegate, DeviceEvent, DeviceRuntime, Handshake, Message, ReportType, device_data,
};
use crate::sdp::service::{SdpRequestHandler, SdpServer};
use crate::sdp::{DataElement, SdpPdu, ServiceAttribute, error_code};
use crate::{BluetoothUuid, Channel, HidServiceConfig};

pub(crate) const SDP_PSM: u32 = 0x0001;
pub(crate) const HID_CONTROL_PSM: u32 = 0x0011;
pub(crate) const HID_INTERRUPT_PSM: u32 = 0x0013;
pub(crate) const HID_SERVICE_RECORD_HANDLE: u32 = 0x0001_0001;

const HID_SERVICE_CLASS_UUID: u16 = 0x1124;
const L2CAP_PROTOCOL_UUID: u16 = 0x0100;
const HIDP_PROTOCOL_UUID: u16 = 0x0011;
const PUBLIC_BROWSE_ROOT_UUID: u16 = 0x1002;

#[derive(Debug)]
pub(crate) struct HidSdpChannel {
    server: SdpServer,
}

impl HidSdpChannel {
    pub(crate) fn new(configuration: &HidServiceConfig, peer_mtu: u16) -> Self {
        let mut server = SdpServer::new(peer_mtu);
        server.add_service(
            HID_SERVICE_RECORD_HANDLE,
            build_hid_service_record(configuration),
        );
        Self { server }
    }

    pub(crate) fn handle_sdu(&mut self, sdu: &[u8]) -> Option<Vec<u8>> {
        let transaction_id = sdu
            .get(1..3)
            .map(|bytes| u16::from_be_bytes([bytes[0], bytes[1]]))?;
        let response = match parse_complete_pdu(sdu) {
            Ok(request) => self.server.handle_request(&request),
            Err(()) => SdpPdu::ErrorResponse {
                transaction_id,
                error_code: error_code::INVALID_REQUEST_SYNTAX,
            },
        };
        response.to_bytes().ok()
    }
}

fn parse_complete_pdu(sdu: &[u8]) -> Result<SdpPdu, ()> {
    let length_bytes = sdu.get(3..5).ok_or(())?;
    let parameter_length = usize::from(u16::from_be_bytes([length_bytes[0], length_bytes[1]]));
    if sdu.len() != 5 + parameter_length {
        return Err(());
    }
    SdpPdu::from_bytes(sdu).map_err(|_| ())
}

fn build_hid_service_record(configuration: &HidServiceConfig) -> Vec<ServiceAttribute> {
    let policy = configuration.sdp_policy();
    let mut attributes = vec![
        attribute(
            0x0000,
            DataElement::unsigned_integer_32(HID_SERVICE_RECORD_HANDLE),
        ),
        attribute(
            0x0001,
            DataElement::sequence([DataElement::uuid(uuid16(HID_SERVICE_CLASS_UUID))]),
        ),
        attribute(
            0x0004,
            DataElement::sequence([
                DataElement::sequence([
                    DataElement::uuid(uuid16(L2CAP_PROTOCOL_UUID)),
                    DataElement::unsigned_integer_16(HID_CONTROL_PSM as u16),
                ]),
                DataElement::sequence([DataElement::uuid(uuid16(HIDP_PROTOCOL_UUID))]),
            ]),
        ),
        attribute(
            0x0005,
            DataElement::sequence([DataElement::uuid(uuid16(PUBLIC_BROWSE_ROOT_UUID))]),
        ),
        attribute(
            0x0006,
            DataElement::sequence([
                DataElement::unsigned_integer_16(0x656E),
                DataElement::unsigned_integer_16(0x006A),
                DataElement::unsigned_integer_16(0x0100),
            ]),
        ),
        attribute(
            0x0100,
            DataElement::text_string(policy.service_name.as_bytes()),
        ),
    ];
    if let Some(description) = &policy.service_description {
        attributes.push(attribute(
            0x0101,
            DataElement::text_string(description.as_bytes()),
        ));
    }
    if let Some(provider) = &policy.provider_name {
        attributes.push(attribute(
            0x0102,
            DataElement::text_string(provider.as_bytes()),
        ));
    }
    attributes.extend([
        attribute(
            0x0009,
            DataElement::sequence([DataElement::sequence([
                DataElement::uuid(uuid16(HID_SERVICE_CLASS_UUID)),
                DataElement::unsigned_integer_16(policy.bluetooth_profile_version),
            ])]),
        ),
        attribute(
            0x000D,
            DataElement::sequence([DataElement::sequence([
                DataElement::sequence([
                    DataElement::uuid(uuid16(L2CAP_PROTOCOL_UUID)),
                    DataElement::unsigned_integer_16(HID_INTERRUPT_PSM as u16),
                ]),
                DataElement::sequence([DataElement::uuid(uuid16(HIDP_PROTOCOL_UUID))]),
            ])]),
        ),
    ]);
    if let Some(device_release_number) = policy.device_release_number {
        attributes.push(attribute(
            0x0200,
            DataElement::unsigned_integer_16(device_release_number),
        ));
    }
    attributes.extend([
        attribute(
            0x0201,
            DataElement::unsigned_integer_16(policy.parser_version),
        ),
        attribute(
            0x0202,
            DataElement::unsigned_integer_8(policy.device_subclass),
        ),
        attribute(0x0203, DataElement::unsigned_integer_8(policy.country_code)),
        attribute(0x0204, DataElement::boolean(policy.virtual_cable)),
        attribute(0x0205, DataElement::boolean(policy.reconnect_initiate)),
        attribute(
            0x0206,
            DataElement::sequence([DataElement::sequence([
                DataElement::unsigned_integer_8(0x22),
                DataElement::text_string(configuration.report_descriptor()),
            ])]),
        ),
        attribute(
            0x0207,
            DataElement::sequence([DataElement::sequence([
                DataElement::unsigned_integer_16(0x0409),
                DataElement::unsigned_integer_16(0x0100),
            ])]),
        ),
    ]);
    if let Some(remote_wake) = policy.remote_wake {
        attributes.push(attribute(0x020A, DataElement::boolean(remote_wake)));
    }
    attributes.extend([
        attribute(
            0x020B,
            DataElement::unsigned_integer_16(policy.profile_version),
        ),
        attribute(
            0x020C,
            DataElement::unsigned_integer_16(policy.supervision_timeout),
        ),
        attribute(0x020D, DataElement::boolean(policy.normally_connectable)),
        attribute(0x020E, DataElement::boolean(policy.boot_device)),
        attribute(
            0x020F,
            DataElement::unsigned_integer_16(policy.ssr_host_max_latency),
        ),
        attribute(
            0x0210,
            DataElement::unsigned_integer_16(policy.ssr_host_min_timeout),
        ),
    ]);
    attributes
}

fn attribute(id: u16, value: DataElement) -> ServiceAttribute {
    ServiceAttribute::new(id, value)
}

fn uuid16(value: u16) -> BluetoothUuid {
    BluetoothUuid::from_u16(value)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum HidpBridgeEvent {
    Output {
        channel: Channel,
        payload: Box<[u8]>,
    },
    ControlResponse(Box<[u8]>),
    Suspend,
    Resume,
    VirtualCableUnplug,
    Unsupported {
        channel: Channel,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HidpBridgeError {
    Malformed {
        channel: Channel,
    },
    EncodeFailed {
        channel: Channel,
    },
    PeerMtuExceeded {
        channel: Channel,
        encoded_len: usize,
        peer_mtu: usize,
    },
}

impl fmt::Display for HidpBridgeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for HidpBridgeError {}

#[derive(Debug, Default)]
struct UnsupportedDelegate;

impl DeviceDelegate for UnsupportedDelegate {}

pub(crate) struct HidpBridge {
    runtime: DeviceRuntime<UnsupportedDelegate>,
    control_peer_mtu: usize,
    interrupt_peer_mtu: usize,
}

impl HidpBridge {
    pub(crate) fn new(control_peer_mtu: usize, interrupt_peer_mtu: usize) -> Self {
        Self {
            runtime: DeviceRuntime::new(UnsupportedDelegate, control_peer_mtu),
            control_peer_mtu,
            interrupt_peer_mtu,
        }
    }

    pub(crate) fn handle(
        &mut self,
        channel: Channel,
        bytes: &[u8],
    ) -> Result<Vec<HidpBridgeEvent>, HidpBridgeError> {
        let device_events = match channel {
            Channel::Control => self
                .runtime
                .handle_control(bytes)
                .map_err(|_| HidpBridgeError::Malformed { channel })?,
            Channel::Interrupt => vec![
                self.runtime
                    .handle_interrupt(bytes)
                    .map_err(|_| HidpBridgeError::Malformed { channel })?,
            ],
        };
        let mut events = Vec::with_capacity(device_events.len());
        for event in device_events {
            self.translate_event(channel, event, &mut events)?;
        }
        Ok(events)
    }

    pub(crate) fn encode_input(&self, payload: &[u8]) -> Result<Box<[u8]>, HidpBridgeError> {
        let encoded = device_data(payload.to_vec()).to_bytes().map_err(|_| {
            HidpBridgeError::EncodeFailed {
                channel: Channel::Interrupt,
            }
        })?;
        check_peer_mtu(Channel::Interrupt, encoded, self.interrupt_peer_mtu)
    }

    pub(crate) fn set_peer_mtu(&mut self, channel: Channel, peer_mtu: usize) {
        match channel {
            Channel::Control => self.control_peer_mtu = peer_mtu,
            Channel::Interrupt => self.interrupt_peer_mtu = peer_mtu,
        }
    }

    pub(crate) fn invalid_parameter_response(&self) -> Result<Box<[u8]>, HidpBridgeError> {
        self.encode_control(Message::Handshake(Handshake::ERR_INVALID_PARAMETER))
    }

    fn translate_event(
        &self,
        channel: Channel,
        event: DeviceEvent,
        output: &mut Vec<HidpBridgeEvent>,
    ) -> Result<(), HidpBridgeError> {
        match event {
            DeviceEvent::SendControl(message) => {
                output.push(HidpBridgeEvent::ControlResponse(
                    self.encode_control(message)?,
                ));
            }
            DeviceEvent::ControlData { report_type, data }
            | DeviceEvent::InterruptData { report_type, data } => {
                output.push(if report_type == ReportType::OUTPUT_REPORT {
                    HidpBridgeEvent::Output {
                        channel,
                        payload: data.into_boxed_slice(),
                    }
                } else {
                    HidpBridgeEvent::Unsupported { channel }
                });
            }
            DeviceEvent::Suspend => output.push(HidpBridgeEvent::Suspend),
            DeviceEvent::ExitSuspend => output.push(HidpBridgeEvent::Resume),
            DeviceEvent::VirtualCableUnplug => output.push(HidpBridgeEvent::VirtualCableUnplug),
            DeviceEvent::Unsupported(_) => {
                output.push(HidpBridgeEvent::Unsupported { channel });
            }
        }
        Ok(())
    }

    fn encode_control(&self, message: Message) -> Result<Box<[u8]>, HidpBridgeError> {
        let encoded = message
            .to_bytes()
            .map_err(|_| HidpBridgeError::EncodeFailed {
                channel: Channel::Control,
            })?;
        check_peer_mtu(Channel::Control, encoded, self.control_peer_mtu)
    }
}

fn check_peer_mtu(
    channel: Channel,
    encoded: Vec<u8>,
    peer_mtu: usize,
) -> Result<Box<[u8]>, HidpBridgeError> {
    if encoded.len() > peer_mtu {
        return Err(HidpBridgeError::PeerMtuExceeded {
            channel,
            encoded_len: encoded.len(),
            peer_mtu,
        });
    }
    Ok(encoded.into_boxed_slice())
}
