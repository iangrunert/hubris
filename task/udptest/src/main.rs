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

    const SOCKET: SocketName = SocketName::rcv;

    let user_leds = drv_user_leds_api::UserLeds::from(USER_LEDS.get_task_id());

    loop {
        // Tiiiiiny payload buffer
        let mut rx_data_buf = [0u8; 64];
        match net.recv_packet(
            SOCKET,
            LargePayloadBehavior::Discard,
            &mut rx_data_buf,
        ) {
            Ok(_) => {
                // A packet! We want to turn it right around. Deserialize the
                // packet header; unwrap because we trust the server.
                let disp_val = UDP_ECHO_COUNT
                    .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                
                // TODO 
                // Is there a way to avoid two identical match statements here,
                // one for led_on and one for led_off?
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

static UDP_ECHO_COUNT: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(0);

include!(concat!(env!("OUT_DIR"), "/notifications.rs"));
