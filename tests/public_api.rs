use std::sync::Arc;

use swbt_bumble_backend::{
    ActivityNotifier, AdapterSelector, AddressKind, BluetoothAddress, BondStore, BondStoreError,
    Channel, ClassicBond, ErrorKind, Event, HidSdpPolicy, HidServiceConfig, LocalIdentity,
    OpenOptions, Session, SessionConfig,
};

fn hid_service() -> HidServiceConfig {
    HidServiceConfig::new(
        [0x05, 0x01, 0x09, 0x05],
        HidSdpPolicy {
            service_name: "Wireless Gamepad".into(),
            service_description: Some("Gamepad".into()),
            provider_name: Some("swbt-rs".into()),
            device_release_number: Some(0x0100),
            bluetooth_profile_version: 0x0101,
            parser_version: 0x0111,
            device_subclass: 0x08,
            country_code: 0,
            virtual_cable: true,
            reconnect_initiate: true,
            remote_wake: Some(true),
            profile_version: 0x0101,
            supervision_timeout: 0x0c80,
            normally_connectable: true,
            boot_device: false,
            ssr_host_max_latency: 0x0640,
            ssr_host_min_timeout: 0x0320,
        },
    )
}

#[test]
fn public_configuration_builds_complete_local_name_eir_without_protocol_types() {
    let config = SessionConfig::new("Wireless Gamepad", 0x0000_2508, hid_service()).unwrap();
    let options = OpenOptions::new(
        AdapterSelector::from("usb:0a12:0001/secret-serial"),
        config,
        ActivityNotifier::new(|| {}),
    )
    .with_local_identity(LocalIdentity::Explicit(
        BluetoothAddress::parse("02:12:34:56:78:9A", AddressKind::Public).unwrap(),
    ));

    assert_eq!(options.config().local_name(), "Wireless Gamepad");
    assert_eq!(options.config().class_of_device(), 0x0000_2508);
    assert_eq!(
        options.config().complete_local_name_eir(),
        [
            17, 0x09, b'W', b'i', b'r', b'e', b'l', b'e', b's', b's', b' ', b'G', b'a', b'm', b'e',
            b'p', b'a', b'd'
        ]
    );
    assert_eq!(
        options.config().hid_service().report_descriptor(),
        &[0x05, 0x01, 0x09, 0x05]
    );
    assert_eq!(
        options.local_identity(),
        LocalIdentity::Explicit(
            BluetoothAddress::parse("02:12:34:56:78:9A", AddressKind::Public).unwrap()
        )
    );

    let rendered = format!("{:?}", options.adapter());
    assert!(rendered.contains("<redacted>"));
    assert!(!rendered.contains("secret-serial"));
}

#[test]
fn invalid_controller_identity_configuration_returns_a_typed_error() {
    for result in [
        SessionConfig::new("", 0x0000_2508, hid_service()),
        SessionConfig::new("x".repeat(239), 0x0000_2508, hid_service()),
        SessionConfig::new("Wireless Gamepad", 0x0100_0000, hid_service()),
    ] {
        assert_eq!(result.unwrap_err().kind(), ErrorKind::InvalidConfiguration);
    }
}

#[test]
fn session_facing_types_are_send_and_do_not_require_bumble_protocol_types() {
    fn assert_send<T: Send>() {}
    fn assert_sync<T: Sync>() {}

    assert_send::<Session>();
    assert_send::<Box<dyn BondStore>>();
    assert_sync::<ActivityNotifier>();

    let peer = BluetoothAddress::from_le_bytes([6, 5, 4, 3, 2, 1], AddressKind::Public);
    let events = [
        Event::Connected { peer },
        Event::ChannelOpened {
            channel: Channel::Control,
        },
        Event::HidOutput {
            channel: Channel::Interrupt,
            payload: Box::from([0x01, 0x02]),
        },
        Event::Disconnected { reason: Some(0x13) },
    ];
    assert_eq!(events.len(), 4);
}

#[derive(Default)]
struct TestBondStore;

impl BondStore for TestBondStore {
    fn load(&self, _peer: BluetoothAddress) -> Result<Option<ClassicBond>, BondStoreError> {
        Ok(None)
    }

    fn load_all(&self) -> Result<Vec<(BluetoothAddress, ClassicBond)>, BondStoreError> {
        Ok(Vec::new())
    }

    fn upsert(
        &mut self,
        _peer: BluetoothAddress,
        _bond: ClassicBond,
    ) -> Result<(), BondStoreError> {
        Ok(())
    }
}

#[test]
fn bond_store_and_activity_callback_are_owned_session_inputs() {
    let store: Box<dyn BondStore> = Box::<TestBondStore>::default();
    assert!(store.load_all().unwrap().is_empty());

    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = calls.clone();
    let notifier = ActivityNotifier::new(move || {
        observed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    });
    notifier.notify();
    assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);

    let secret = "A5".repeat(16);
    let rendered = format!("{:?}", ClassicBond::new([0xA5; 16], 4, true));
    assert!(rendered.contains("<redacted>"));
    assert!(!rendered.contains(&secret));
}
