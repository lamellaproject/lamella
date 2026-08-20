//! The Lamella Link relay daemon.

use std::net::TcpListener;
use std::time::Duration;

use lamella_linkd::{IDLE_SLEEP, Options, USAGE, relay_session};
use lamella_wire::relay::Side;
use lamella_wire_host::{TcpTransport, open_target};

/// How long to wait for the device carrier to open.
///
/// Applies only to a network device target; a serial open is immediate and ignores it. Generous
/// rather than tight: a companion processor bringing up its own link to the board can be slower
/// than a desktop, and the cost of waiting is a slower error while the cost of giving up early is
/// an error that is not true.
const DEVICE_OPEN_TIMEOUT: Duration = Duration::from_secs(5);

fn main() {
    let options = match Options::parse(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("lamella-linkd: {error}");
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    };

    match open_target(&options.device, options.baud, DEVICE_OPEN_TIMEOUT) {
        Ok(_) => eprintln!("lamella-linkd: device {} opens at {} baud", options.device, options.baud),
        Err(error) => {
            eprintln!(
                "lamella-linkd: cannot open device {} at {} baud: {error:?}",
                options.device, options.baud
            );
            std::process::exit(1);
        }
    }

    let listener = match TcpListener::bind(&options.listen) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("lamella-linkd: cannot listen on {}: {error}", options.listen);
            std::process::exit(1);
        }
    };
    eprintln!("lamella-linkd: listening on {} for one host at a time", options.listen);

    loop {
        let (stream, peer) = match listener.accept() {
            Ok(accepted) => accepted,
            Err(error) => {
                eprintln!("lamella-linkd: accept failed: {error}");
                continue;
            }
        };
        eprintln!("lamella-linkd: {peer} connected");

        let mut host = match TcpTransport::from_stream(stream) {
            Ok(transport) => transport,
            Err(error) => {
                eprintln!("lamella-linkd: {peer} could not be set up as a carrier: {error:?}");
                continue;
            }
        };
        let mut device = match open_target(&options.device, options.baud, DEVICE_OPEN_TIMEOUT) {
            Ok(transport) => transport,
            Err(error) => {
                eprintln!("lamella-linkd: device {} would not open: {error:?}", options.device);
                eprintln!("lamella-linkd: dropping {peer}; nothing was relayed");
                continue;
            }
        };

        if let Err(error) = listener.set_nonblocking(true) {
            eprintln!("lamella-linkd: could not watch for extra hosts: {error}");
        }
        let refuse_extras = || {
            while let Ok((extra, extra_peer)) = listener.accept() {
                eprintln!("lamella-linkd: refusing {extra_peer}; {peer} holds the line");
                drop(extra);
            }
            true
        };

        let fault = relay_session(&mut host, &mut device, refuse_extras, || {
            std::thread::sleep(IDLE_SLEEP);
        });

        if let Err(error) = listener.set_nonblocking(false) {
            eprintln!("lamella-linkd: could not return to blocking accept: {error}");
            std::process::exit(1);
        }

        match fault {
            Some(fault) if fault.side == Side::Host => {
                eprintln!("lamella-linkd: {peer} went away ({:?}); the line is free", fault.error);
            }
            Some(fault) => {
                eprintln!(
                    "lamella-linkd: the DEVICE stopped answering ({:?}) -- the board needs \
                     attention, not the host",
                    fault.error
                );
            }
            None => eprintln!("lamella-linkd: session with {peer} ended"),
        }
    }
}
