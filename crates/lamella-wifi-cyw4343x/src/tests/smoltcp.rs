//! The device adapter over the fake transport: the tokens' discipline, the
//! stack's reply to an ARP request built inside the receive and written as
//! a data frame, the capabilities and the hardware address.

use super::data::{
    BURST, arp_reply, arp_reply_from_us, arp_request, arp_request_from_ap, data_recv, data_send,
    joined_over, joined_script,
};
use super::station::{STATION, burst_ops};
use crate::data::ETHERNET_MAX;
use crate::driver::Outcome;
use crate::fixture::run;
use crate::link::LinkState;
use crate::smoltcp::Adapter;
use ::smoltcp::iface::{Config, Interface, SocketSet, SocketStorage};
use ::smoltcp::phy::{Device, Medium, RxToken, TxToken};
use ::smoltcp::time::Instant;
use ::smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr};
use std::vec::Vec;

const CAP: u32 = 100_000;

#[test]
fn the_tokens_hand_the_frame_in_place_and_stage_the_reply() {
    let (mut script, mut c) = joined_script();
    data_recv(&mut script, &mut c, true, 14, 0, &arp_reply());
    data_send(&mut script, &mut c, &arp_request());
    let (mut driver, mut bus, mut clock) = joined_over(&script);
    {
        let mut adapter = Adapter::new(&mut driver, &mut bus);
        assert!(adapter.receive(Instant::ZERO).is_none(), "nothing waiting");
        assert!(
            adapter.transmit(Instant::ZERO).is_some(),
            "the window open, nothing staged"
        );
    }
    assert!(!driver.sending(), "a token dropped unused stages nothing");
    let r = run(&mut driver, &mut bus, &mut clock, 1);
    assert_eq!(r.outcome, Some(Outcome::Frame { len: 42 }));
    {
        let mut adapter = Adapter::new(&mut driver, &mut bus);
        let (rx, _) = adapter.receive(Instant::ZERO).expect("the frame");
        let got: Vec<u8> = rx.consume(|frame| frame.to_vec());
        assert_eq!(got, arp_reply());
        assert!(adapter.receive(Instant::ZERO).is_none(), "taken");
        let tx = adapter
            .transmit(Instant::ZERO)
            .expect("the window open, nothing staged");
        let n = tx.consume(42, |slot| {
            slot.copy_from_slice(&arp_request());
            slot.len()
        });
        assert_eq!(n, 42);
    }
    assert!(!driver.sending(), "written");
    assert_eq!(driver.frames().data_sent, 1);
    assert_eq!(driver.credit(), (20, 41));
    assert_eq!(bus.remaining(), 0);
    assert_eq!(bus.fault(), None);
}

#[test]
fn no_token_while_the_window_is_closed_a_frame_is_staged_or_the_link_is_down() {
    // The window (19, 40) after the join: twenty-one sends close it.
    let (mut script, mut c) = joined_script();
    for _ in 0..BURST - 1 {
        data_send(&mut script, &mut c, &arp_request());
    }
    data_recv(&mut script, &mut c, true, 14, 0, &arp_reply());
    let (mut driver, mut bus, mut clock) = joined_over(&script);
    for _ in 0..BURST - 1 {
        assert_eq!(driver.send(&mut bus, &arp_request()), Ok(true));
    }
    assert_eq!(driver.credit(), (40, 40), "closed");
    {
        let mut adapter = Adapter::new(&mut driver, &mut bus);
        assert!(
            adapter.transmit(Instant::ZERO).is_none(),
            "the window closed"
        );
    }
    assert_eq!(
        driver.send(&mut bus, &arp_request()),
        Ok(true),
        "taken and held"
    );
    assert!(driver.sending());
    {
        let mut adapter = Adapter::new(&mut driver, &mut bus);
        assert!(adapter.transmit(Instant::ZERO).is_none(), "a frame staged");
    }
    let r = run(&mut driver, &mut bus, &mut clock, 1);
    assert_eq!(r.outcome, Some(Outcome::Frame { len: 42 }));
    {
        let mut adapter = Adapter::new(&mut driver, &mut bus);
        assert!(
            adapter.receive(Instant::ZERO).is_none(),
            "a frame staged: the stack could not reply"
        );
    }
    assert_eq!(
        driver.frame_payload(),
        &arp_reply()[..],
        "readable all the same"
    );
    assert_eq!(bus.remaining(), 0);

    let (mut script, mut c) = joined_script();
    script.extend(burst_ops(&mut c, true));
    let (mut driver, mut bus, mut clock) = joined_over(&script);
    assert!(matches!(
        run(&mut driver, &mut bus, &mut clock, CAP).outcome,
        Some(Outcome::LinkLost(_))
    ));
    assert_eq!(driver.link(), LinkState::Down);
    {
        let mut adapter = Adapter::new(&mut driver, &mut bus);
        assert!(adapter.transmit(Instant::ZERO).is_none(), "the link down");
        assert!(adapter.receive(Instant::ZERO).is_none());
    }
}

#[test]
fn the_stacks_arp_reply_is_built_inside_the_receive_and_written_as_a_data_frame() {
    let (mut script, mut c) = joined_script();
    data_recv(&mut script, &mut c, true, 14, 0, &arp_request_from_ap());
    data_send(&mut script, &mut c, &arp_reply_from_us());
    let (mut driver, mut bus, mut clock) = joined_over(&script);
    let address = driver.address().expect("the radio up");
    let config = Config::new(HardwareAddress::Ethernet(EthernetAddress(address)));
    let mut iface = {
        let mut adapter = Adapter::new(&mut driver, &mut bus);
        assert_eq!(
            adapter.hardware_address(),
            Some(HardwareAddress::Ethernet(EthernetAddress(STATION)))
        );
        Interface::new(config, &mut adapter, Instant::ZERO)
    };
    iface.update_ip_addrs(|addrs| {
        addrs
            .push(IpCidr::new(IpAddress::v4(192, 168, 4, 2), 24))
            .expect("one address");
    });
    let mut storage: [SocketStorage; 0] = [];
    let mut sockets = SocketSet::new(&mut storage[..]);
    let r = run(&mut driver, &mut bus, &mut clock, 1);
    assert_eq!(r.outcome, Some(Outcome::Frame { len: 42 }));
    assert_eq!(driver.frame_payload(), &arp_request_from_ap()[..]);
    {
        let mut adapter = Adapter::new(&mut driver, &mut bus);
        let _ = iface.poll(
            Instant::from_micros(clock.now() as i64),
            &mut adapter,
            &mut sockets,
        );
    }
    assert_eq!(
        bus.remaining(),
        0,
        "the reply written as the hand-derived data frame"
    );
    assert_eq!(bus.fault(), None);
    assert_eq!(driver.frames().data_sent, 1);
    assert!(!driver.sending());
    {
        let mut adapter = Adapter::new(&mut driver, &mut bus);
        assert!(
            adapter.receive(Instant::ZERO).is_none(),
            "taken by the stack"
        );
    }
    assert_eq!(
        driver.frame_payload(),
        &arp_request_from_ap()[..],
        "readable until the next pass"
    );
}

#[test]
fn the_capabilities_and_the_hardware_address() {
    let (script, _) = joined_script();
    let (mut driver, mut bus, _) = joined_over(&script);
    let adapter = Adapter::new(&mut driver, &mut bus);
    let capabilities = adapter.capabilities();
    assert_eq!(capabilities.medium, Medium::Ethernet);
    assert_eq!(capabilities.max_transmission_unit, ETHERNET_MAX);
    assert_eq!(capabilities.max_burst_size, Some(1));
    assert_eq!(
        adapter.hardware_address(),
        Some(HardwareAddress::Ethernet(EthernetAddress(STATION)))
    );
    assert_eq!(adapter.driver().link(), LinkState::Up);
    assert_eq!(adapter.driver().address(), Some(STATION));
}
