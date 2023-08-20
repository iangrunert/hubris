// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![no_std]
#![no_main]

use task_net_api::*;
use userlib::*;

task_slot!(NET, net);
task_slot!(USER_LEDS, user_leds);

#[export_name = "main"]
fn main() -> ! {
    let net = NET.get_task_id();
    let net = Net::from(net);

    // Incoming socket for DNS requests
    const SOCKET: SocketName = SocketName::dns;
    // Outgoing socket (may want a pool of sockets)
    // We need a socket to use when querying the upstream, so we can wait for 
    // a response on that socket. Most operating systems would open an
    // ephemeral port for this, in Hubris we need a port defined ahead of time
    const OUTBOUND_SOCKET: SocketName = SocketName::dnsupstream;

    let user_leds = drv_user_leds_api::UserLeds::from(USER_LEDS.get_task_id());

    loop {
        // payload buffer, big enough for UDP DNS requests
        let mut rx_data_buf = [0u8; 512];
        match net.recv_packet(
            SOCKET,
            LargePayloadBehavior::Discard,
            &mut rx_data_buf,
        ) {
            Ok(meta) => {
                // A packet! Let's start by showing the updated packet count on the LEDs
                let disp_val = UDP_RCV_COUNT
                    .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                
                // Adding led_set would simplify this code
                // https://github.com/oxidecomputer/hubris/issues/430
                let mut current = 3;
                while current > 0 {
                    current -= 1;
                    if disp_val & (1 << current) != 0 {
                        match user_leds.led_on(current) {
                            Ok(_) => continue,
                            Err(drv_user_leds_api::LedError::NotPresent) => sys_panic(b"unexpected non-fault!"),
                        }
                    } else {
                        match user_leds.led_off(current) {
                            Ok(_) => continue,
                            Err(drv_user_leds_api::LedError::NotPresent) => sys_panic(b"unexpected non-fault!"),
                        }
                    }
                }

                let tx_bytes = &rx_data_buf[..meta.size as usize];
                loop {
                    match net.send_packet(SOCKET, meta, tx_bytes) {
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
                    }
                }

                // TODO figure out approach for locking / exclusive lease on OUTBOUND_SOCKET
                // TODO pool of outbound sockets, to avoid blocking here

                // Make an upstream call to 192.168.0.1 resolve
                // let mut upstream_req = meta.clone();
                // upstream_req.addr = Address::Ipv4(Ipv4Address([
                //     0xc0, 0xa8, 0x00, 0x01
                // ]));
                // let tx_bytes = &rx_data_buf[..meta.size as usize];

                // loop {
                //     match net.send_packet(OUTBOUND_SOCKET, upstream_req, tx_bytes) {
                //         Ok(()) => {
                //             // Wait for the response
                //             // TODO need a timeout here so we can return OUTBOUND_SOCKET if
                //             // the server never responds
                //             let mut rcv_data_buf = [0u8; 512];
                //             loop {
                //                 match net.recv_packet(
                //                     OUTBOUND_SOCKET,
                //                     LargePayloadBehavior::Discard,
                //                     &mut rcv_data_buf,
                //                 ) {
                //                     Ok(_) => {
                //                         // Return the response back to the caller
                //                         let resp_bytes = &rcv_data_buf;

                //                         loop {
                //                             match net.send_packet(SOCKET, meta, resp_bytes) {
                //                                 Ok(()) => break,
                //                                 Err(SendError::QueueFull) => {
                //                                     // Our outgoing queue is full; wait for space.
                //                                     sys_recv_closed(
                //                                         &mut [],
                //                                         notifications::SOCKET_MASK,
                //                                         TaskId::KERNEL,
                //                                     )
                //                                     .unwrap();
                //                                 }
                //                                 Err(
                //                                     SendError::ServerRestarted
                //                                     | SendError::NotYours
                //                                     | SendError::InvalidVLan
                //                                     | SendError::Other,
                //                                 ) => panic!(),
                //                             }
                //                         }
                //                     }
                //                     Err(RecvError::QueueEmpty) => {
                //                         // Our incoming queue is empty. Wait for more packets.
                //                         sys_recv_closed(
                //                             &mut [],
                //                             notifications::SOCKET_MASK,
                //                             TaskId::KERNEL,
                //                         )
                //                         .unwrap();
                //                     }
                //                     Err(RecvError::ServerRestarted) => {
                //                         // `net` restarted (probably due to the watchdog); just retry.
                //                     }
                //                     Err(RecvError::NotYours) => panic!(),
                //                     Err(RecvError::Other) => panic!(),                        
                //                 }
                //             }
                //         },
                //         Err(SendError::QueueFull) => {
                //             // Our outgoing queue is full; wait for space.
                //             sys_recv_closed(
                //                 &mut [],
                //                 notifications::SOCKET_MASK,
                //                 TaskId::KERNEL,
                //             )
                //             .unwrap();
                //         }
                //         Err(
                //             SendError::ServerRestarted
                //             | SendError::NotYours
                //             | SendError::InvalidVLan
                //             | SendError::Other,
                //         ) => panic!(),
                //     }
                // }
            }
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
        }

        // Try again.
    }
}

static UDP_RCV_COUNT: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(0);

include!(concat!(env!("OUT_DIR"), "/notifications.rs"));
