// Ported from bumble-l2cap at cb55e2d.

use super::CONNECTION_REFUSED_PSM_NOT_SUPPORTED;
use super::classic::{ChannelManager, ClassicChannelSpec, ClassicChannelState};

#[test]
fn default_information_capabilities_do_not_advertise_le_signaling() {
    let capabilities = super::InformationCapabilities::default();

    assert_eq!(
        capabilities.fixed_channels(),
        1_u64 << super::L2CAP_SIGNALING_CID
    );
    assert_eq!(
        capabilities.fixed_channels() & (1_u64 << super::L2CAP_LE_SIGNALING_CID),
        0
    );
}

fn relay(left: &mut ChannelManager, right: &mut ChannelManager) {
    for _ in 0..32 {
        let mut progressed = false;
        while let Some(pdu) = left.poll_outbound() {
            right.process_pdu(pdu).unwrap();
            progressed = true;
        }
        while let Some(pdu) = right.poll_outbound() {
            left.process_pdu(pdu).unwrap();
            progressed = true;
        }
        if !progressed {
            return;
        }
    }
    panic!("relay did not quiesce");
}

#[test]
fn classic_channel_connect_configure_transfer_and_disconnect() {
    let mut client = ChannelManager::new();
    let mut server = ChannelManager::new();
    let psm = server
        .register_server(Some(0x0003), ClassicChannelSpec { mtu: 345 })
        .unwrap();

    let client_cid = client
        .connect(psm, ClassicChannelSpec { mtu: 456 })
        .unwrap();
    relay(&mut client, &mut server);

    let server_cid = server.poll_accepted_channel().unwrap();
    let client_channel = client.channel(client_cid).unwrap();
    let server_channel = server.channel(server_cid).unwrap();
    assert_eq!(client_channel.state, ClassicChannelState::Open);
    assert_eq!(server_channel.state, ClassicChannelState::Open);
    assert_eq!(client_channel.peer_mtu, 345);
    assert_eq!(server_channel.peer_mtu, 456);
    assert_eq!(client_channel.destination_cid, server_cid);
    assert_eq!(server_channel.destination_cid, client_cid);

    client
        .send(client_cid, b"RFCOMM over Classic L2CAP")
        .unwrap();
    server.send(server_cid, b"SDP reply").unwrap();
    relay(&mut client, &mut server);
    assert_eq!(
        server.channel_mut(server_cid).unwrap().pop_received(),
        Some(b"RFCOMM over Classic L2CAP".to_vec())
    );
    assert_eq!(
        client.channel_mut(client_cid).unwrap().pop_received(),
        Some(b"SDP reply".to_vec())
    );

    assert!(client.send(client_cid, &vec![0; 346]).is_err());
    client.disconnect(client_cid).unwrap();
    relay(&mut client, &mut server);
    assert_eq!(
        client.channel(client_cid).unwrap().state,
        ClassicChannelState::Closed
    );
    assert_eq!(
        server.channel(server_cid).unwrap().state,
        ClassicChannelState::Closed
    );
}

#[test]
fn connection_to_unregistered_psm_is_refused() {
    let mut client = ChannelManager::new();
    let mut server = ChannelManager::new();
    let client_cid = client
        .connect(0x1001, ClassicChannelSpec::default())
        .unwrap();
    relay(&mut client, &mut server);
    let channel = client.channel(client_cid).unwrap();
    assert_eq!(channel.state, ClassicChannelState::Closed);
    assert_eq!(
        channel.connection_result,
        Some(CONNECTION_REFUSED_PSM_NOT_SUPPORTED)
    );
    assert!(server.poll_accepted_channel().is_none());
}

#[test]
fn dynamic_psm_allocation_is_valid_and_deterministic() {
    let mut manager = ChannelManager::new();
    assert_eq!(
        manager
            .register_server(None, ClassicChannelSpec::default())
            .unwrap(),
        0x1001
    );
    assert_eq!(
        manager
            .register_server(None, ClassicChannelSpec::default())
            .unwrap(),
        0x1003
    );
    assert!(
        manager
            .register_server(Some(0x1002), ClassicChannelSpec::default())
            .is_err()
    );
    assert!(
        manager
            .register_server(Some(0x1101), ClassicChannelSpec::default())
            .is_err()
    );
}
