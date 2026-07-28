//! The event half of the simulated device: a `lamella_evt` world whose one granted source is
//! the simulated accelerometer's data-ready edge, so a guest that WAITS on a peripheral can be
//! run with no hardware. Pair it with [`crate::simulated_i2c`], whose caveats apply here too:
//! a simulator shows that the mechanism works, never that a real part behaves this way.

use crate::exec::{HostFunc, Trap, Value, World};
use crate::simulated_i2c::{arg_i32, SharedDevice};
use crate::{FuncType, ValType};
use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;

/// `wait_ready`'s kind-specific status: the wait elapsed before a delivery. Data, not an
/// error -- the elapsed-wait outcome, exactly as a NACK is data. `0 = Ok` is universal.
pub const TIMED_OUT: i32 = 1;
/// The one event kind this world defines: readiness observed (the data-ready shape).
pub const KIND_READY: u32 = 0;
/// This grant's event-source handle: the accelerometer data-ready role. Handle 0 is
/// the I2C bus role in the SAME host-minted handle space.
pub const SRC_ACCEL_READY: u32 = 1;

/// How much simulated time one parked pump iteration advances.
const TICK_US: u32 = 1_000;

/// One subscription: its granted (source, kind) pair and how many device landings it has
/// consumed. Its slot is FILLED while the device's counter exceeds `seen`.
struct Subscription {
    source: u32,
    kind: u32,
    seen: u64,
    alive: bool,
}

fn evt_type(params: usize) -> FuncType {
    FuncType { params: vec![ValType::I32; params], results: vec![ValType::I32] }
}

/// Builds the granted `lamella_evt` world over a shared device: `subscribe`, `unsubscribe`
/// and `wait_ready`, with (SRC_ACCEL_READY, KIND_READY) as the entire subscribable grant.
#[must_use]
pub fn world(device: &SharedDevice) -> World {
    let subs: Rc<RefCell<Vec<Subscription>>> = Rc::new(RefCell::new(Vec::new()));

    let sub_dev = Rc::clone(device);
    let sub_state = Rc::clone(&subs);
    let subscribe = move |_mem: &mut [u8], args: &[Value]| -> Result<Option<Value>, Trap> {
        let (source, kind) = (arg_i32(args, 0)?, arg_i32(args, 1)?);
        if (source, kind) != (SRC_ACCEL_READY, KIND_READY) {
            return Err(Trap::Host("evt: ungranted source or kind"));
        }
        let landings = sub_dev.borrow().landings;
        let mut subs = sub_state.borrow_mut();
        for existing in subs.iter_mut() {
            if existing.alive && existing.source == source && existing.kind == kind {
                existing.alive = false;
            }
        }
        subs.push(Subscription { source, kind, seen: landings, alive: true });
        let sub = subs.len() - 1;
        Ok(Some(Value::I32(sub as u32)))
    };

    let unsub_state = Rc::clone(&subs);
    let unsubscribe = move |_mem: &mut [u8], args: &[Value]| -> Result<Option<Value>, Trap> {
        let sub = arg_i32(args, 0)? as usize;
        let mut subs = unsub_state.borrow_mut();
        match subs.get_mut(sub) {
            Some(entry) if entry.alive => {
                entry.alive = false;
                Ok(Some(Value::I32(0)))
            }
            _ => Err(Trap::Host("evt: unknown subscription")),
        }
    };

    let wait_dev = Rc::clone(device);
    let wait_state = Rc::clone(&subs);
    let wait_ready = move |_mem: &mut [u8], args: &[Value]| -> Result<Option<Value>, Trap> {
        let sub = arg_i32(args, 0)? as usize;
        let timeout_ms = arg_i32(args, 1)? as i32;
        if timeout_ms < 0 {
            return Err(Trap::Host("evt: negative timeout"));
        }
        if !wait_state.borrow().get(sub).is_some_and(|entry| entry.alive) {
            return Err(Trap::Host("evt: unknown subscription"));
        }
        let mut elapsed_ms: i32 = 0;
        loop {
            let landings = wait_dev.borrow().landings;
            let mut subs = wait_state.borrow_mut();
            let entry = &mut subs[sub];
            if landings > entry.seen {
                entry.seen = landings;
                return Ok(Some(Value::I32(0)));
            }
            drop(subs);
            if elapsed_ms >= timeout_ms {
                return Ok(Some(Value::I32(TIMED_OUT as u32)));
            }
            wait_dev.borrow_mut().advance_micros(TICK_US);
            elapsed_ms += 1;
        }
    };

    World {
        funcs: vec![
            HostFunc {
                module: String::from("lamella_evt"),
                name: String::from("subscribe"),
                ty: evt_type(2),
                call: Box::new(subscribe),
            },
            HostFunc {
                module: String::from("lamella_evt"),
                name: String::from("unsubscribe"),
                ty: evt_type(1),
                call: Box::new(unsubscribe),
            },
            HostFunc {
                module: String::from("lamella_evt"),
                name: String::from("wait_ready"),
                ty: evt_type(2),
                call: Box::new(wait_ready),
            },
        ],
    }
}

/// The combined grant: the `lamella_i2c` data plane and the `lamella_evt` event half
/// over ONE shared device -- the world the eventful guest instantiates against.
#[must_use]
pub fn eventful_world(device: &SharedDevice) -> World {
    let mut combined = crate::simulated_i2c::world_over(device);
    combined.funcs.extend(world(device).funcs);
    combined
}
