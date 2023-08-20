// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![no_std]
#![no_main]

use task_net_api::*;
use userlib::*;

task_slot!(NET, net);
task_slot!(USER_LEDS, user_leds);

enum DhcpState {
    Discover,
    ReadOffer,
    Request,
    Idle
}

// Yes, I'm building this by hand
// No, it's not ideal, but kinda fun!
const INFORM_HEADER: &[u8] = &[
    // op, htype, hlen, hops,
    // boot request = 1, htype ethernet = 1, hlen mac address is 6, no hops
    0x01, 0x01, 0x06, 0x00,
    // xid (4)
    0x3d, 0x3d, 0x3d, 0x3d,
    // secs (2) flags (2)
    // no time passed yet, broadcast bit set
    0x00, 0x00, 0x10, 0x00,
    // ciaddr (4)
    // client address - we're statically assigned here so fill it in
    // TODO Duplication with task/net/src/main.rs self_assigned_iface_address
    0xc0, 0xa8, 0x00, 0x2a,
    // yiaddr (4)
    // ignored
    0x00, 0x00, 0x00, 0x00,
    // siaddr (4)
    // ignored
    0x00, 0x00, 0x00, 0x00,
    // giaddr (4)
    // ignored
    0x00, 0x00, 0x00, 0x00,
];

// Send a DHCPDISCOVER packet, so the router knows we're here
fn discover(SOCKET: SocketName) -> DhcpState {
    let user_leds = drv_user_leds_api::UserLeds::from(USER_LEDS.get_task_id());

    user_leds.led_on(1).unwrap();
    user_leds.led_off(2).unwrap();
    user_leds.led_off(3).unwrap();

    let net = NET.get_task_id();
    let net = Net::from(net);

    let client_mac: MacAddress = net.get_mac_address();

    const HEADER_LEN: usize = INFORM_HEADER.len();

    let mut request_msg: [u8; 576] = [0; 576];
    // Copy the header across
    request_msg[0..HEADER_LEN].copy_from_slice(INFORM_HEADER);
    // chaddr (16) - first 6 mac address, remaining 10 blank
    request_msg[HEADER_LEN..HEADER_LEN+6].copy_from_slice(&client_mac.0);
    // sname (64) file (128) for a total of 192 blank octets
    // set magic cookie (4 bytes)
    request_msg[HEADER_LEN+16+192..HEADER_LEN+16+192+4].copy_from_slice(&[0x63, 0x82, 0x53, 0x63]);
    // options
    let options_index = HEADER_LEN+16+192+4;
    // set DHCP_DISCOVER
    // code len type
    request_msg[options_index..options_index+3].copy_from_slice(&[0x35, 0x01, 0x01]);
    // requested ip address
    // code len address
    request_msg[options_index+3..options_index+9].copy_from_slice(&[0x32, 0x04, 0xc0, 0xa8, 0x00, 0x2a]);
    // host name
    // code len name
    request_msg[options_index+9..options_index+13].copy_from_slice(&[0x0c, 0x02, 0x68, 0x69]);

    // Not sure if we need anything else?
    request_msg[options_index+13] = 0xff;

    loop {
        let meta = UdpMetadata {
            addr: Address::Ipv4(Ipv4Address([0xff, 0xff, 0xff, 0xff])),
            port: 67,
            size: request_msg.len() as u32,
            #[cfg(feature = "vlan")]
            vid: vid_iter.next().unwrap(),
        };

        match net.send_packet(SOCKET, meta, &request_msg[..]) {
            Ok(()) => return DhcpState::ReadOffer,
            Err(SendError::QueueFull) => {
                // Our outgoing queue is full; wait for space.
                sys_recv_closed(
                    &mut [],
                    notifications::SOCKET_MASK,
                    TaskId::KERNEL,
                )
                .unwrap();
            }
            Err(
                SendError::ServerRestarted
                | SendError::NotYours
                | SendError::InvalidVLan
                | SendError::Other,
            ) => panic!(),
        };
    }

    return DhcpState::Discover;
}

// Wait for the DHCPOFFER packet response from the router
fn readoffer(SOCKET: SocketName) -> DhcpState {
    let user_leds = drv_user_leds_api::UserLeds::from(USER_LEDS.get_task_id());

    user_leds.led_off(1).unwrap();
    user_leds.led_on(2).unwrap();
    user_leds.led_off(3).unwrap();

    let net = NET.get_task_id();
    let net = Net::from(net);

    let mut attempts: u8 = 3;

    while attempts > 0 {
        attempts -= 1;
        hl::sleep_for(100);

        let mut offer_msg: [u8; 576] = [0; 576];

        match net.recv_packet(SOCKET, LargePayloadBehavior::Discard, &mut offer_msg) {
            Ok(_) => {
                // We should probably check the message, but we'll trust for now
                return DhcpState::Request;
            },
            Err(RecvError::QueueEmpty) => {
                // Our incoming queue is empty. Wait for more packets.
                sys_recv_closed(
                    &mut [],
                    notifications::SOCKET_MASK,
                    TaskId::KERNEL,
                )
                .unwrap();
            }
            Err(RecvError::ServerRestarted) => {
                // `net` restarted (probably due to the watchdog); just retry.
            }
            Err(RecvError::NotYours) => panic!(),
            Err(RecvError::Other) => panic!(),
        };
    }
    // Ran out of attempts, send another Discover
    return DhcpState::Discover;
}

// Send a DHCPREQUEST packet, to lock in the address
fn request(SOCKET: SocketName) -> DhcpState {
    let user_leds = drv_user_leds_api::UserLeds::from(USER_LEDS.get_task_id());

    user_leds.led_off(1).unwrap();
    user_leds.led_off(2).unwrap();
    user_leds.led_on(3).unwrap();

    let net = NET.get_task_id();
    let net = Net::from(net);

    let client_mac: MacAddress = net.get_mac_address();

    const HEADER_LEN: usize = INFORM_HEADER.len();

    let mut request_msg: [u8; 576] = [0; 576];
    // Copy the header across
    request_msg[0..HEADER_LEN].copy_from_slice(INFORM_HEADER);
    // Go back and fill in siaddr (4)
    request_msg[20..24].copy_from_slice(&[0xc0, 0xa8, 0x00, 0x01]);
    // chaddr (16) - first 6 mac address, remaining 10 blank
    request_msg[HEADER_LEN..HEADER_LEN+6].copy_from_slice(&client_mac.0);
    // sname (64) file (128) for a total of 192 blank octets
    // set magic cookie (4 bytes)
    request_msg[HEADER_LEN+16+192..HEADER_LEN+16+192+4].copy_from_slice(&[0x63, 0x82, 0x53, 0x63]);
    // options
    let options_index = HEADER_LEN+16+192+4;
    // set DHCP_Request
    // code len type
    request_msg[options_index..options_index+3].copy_from_slice(&[0x35, 0x01, 0x03]);
    // requested ip address
    // code len address
    request_msg[options_index+3..options_index+9].copy_from_slice(&[0x32, 0x04, 0xc0, 0xa8, 0x00, 0x2a]);
    // host name
    // code len name
    request_msg[options_index+9..options_index+13].copy_from_slice(&[0x0c, 0x02, 0x68, 0x69]);
    // dhcp server
    // code len name
    request_msg[options_index+13..options_index+19].copy_from_slice(&[0x36, 0x04, 0xc0, 0xa8, 0x00, 0x01]);
    // Not sure if we need anything else?
    request_msg[options_index+19] = 0xff;

    loop {
        let meta = UdpMetadata {
            addr: Address::Ipv4(Ipv4Address([0xc0, 0xa8, 0x00, 0x01])),
            port: 67,
            size: request_msg.len() as u32,
            #[cfg(feature = "vlan")]
            vid: vid_iter.next().unwrap(),
        };

        match net.send_packet(SOCKET, meta, &request_msg[..]) {
            Ok(()) => break,
            Err(SendError::QueueFull) => {
                // Our outgoing queue is full; wait for space.
                sys_recv_closed(
                    &mut [],
                    notifications::SOCKET_MASK,
                    TaskId::KERNEL,
                )
                .unwrap();
            }
            Err(
                SendError::ServerRestarted
                | SendError::NotYours
                | SendError::InvalidVLan
                | SendError::Other,
            ) => panic!(),
        };
    }

    // Really we should have a ReadAck state
    // This will do for now
    return DhcpState::Idle;
}

fn idle() -> DhcpState {
    let user_leds = drv_user_leds_api::UserLeds::from(USER_LEDS.get_task_id());

    user_leds.led_off(1).unwrap();
    user_leds.led_off(2).unwrap();
    user_leds.led_off(3).unwrap();

    // Refresh every 12 hours
    // hl::sleep_for(1000 * 60 * 60 * 12);
    // Refresh every 30 seconds
    hl::sleep_for(1000 * 30);

    return DhcpState::Discover;
}

#[export_name = "main"]
fn main() -> ! {
    const SOCKET: SocketName = SocketName::dhcp;
    let mut current_state: DhcpState = DhcpState::Discover;

    loop {
        match &current_state {
            DhcpState::Discover => {
                current_state = discover(SOCKET);
            },
            DhcpState::ReadOffer => {
                current_state = readoffer(SOCKET);
            },
            DhcpState::Request => {
                current_state = request(SOCKET);
            },
            DhcpState::Idle => {
                current_state = idle();
            },
        }
    }
}

include!(concat!(env!("OUT_DIR"), "/notifications.rs"));
