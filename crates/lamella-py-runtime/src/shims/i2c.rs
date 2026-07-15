//! The MicroPython `machine.I2C` / CircuitPython `busio.I2C` shim faces -- thin wrappers over the
//! Lamella-standard `i2c.open` + `I2cBus`. The flavor gates the visible names and holds each
//! shimmed API's quirks (machine's 400 kHz default + readfrom_mem/writeto_mem register idiom;
//! busio's lock protocol + writeto_then_readfrom) OUTSIDE the standard surface. The buffer-slice
//! kwargs (`start`/`end`/`stop`) are accepted only at their defaults -- a non-default slice
//! loud-rejects rather than silently reading the wrong span (full support is a follow-up).

use crate::i2c;
use crate::object::ObjectModel;
use crate::trap::Trap;
use crate::value::Value;

/// Which shimmed API a factory/instance wears.
pub(crate) const SHIM_FLAVOR_MACHINE: u32 = 0;
pub(crate) const SHIM_FLAVOR_BUSIO: u32 = 1;

pub(crate) const SHIM_W_BUS: u32 = 0;
pub(crate) const SHIM_W_FLAVOR: u32 = 1;
pub(crate) const SHIM_WORDS: u32 = 2;

pub(crate) const SHIM_SCAN: u32 = 0;
pub(crate) const SHIM_READFROM: u32 = 1;
pub(crate) const SHIM_WRITETO: u32 = 2;
pub(crate) const SHIM_READFROM_INTO: u32 = 3;
pub(crate) const SHIM_READFROM_MEM: u32 = 4;
pub(crate) const SHIM_WRITETO_MEM: u32 = 5;
pub(crate) const SHIM_WRITETO_THEN_READFROM: u32 = 6;
pub(crate) const SHIM_TRY_LOCK: u32 = 7;
pub(crate) const SHIM_UNLOCK: u32 = 8;
pub(crate) const SHIM_DEINIT: u32 = 9;
pub(crate) const SHIM_ENTER: u32 = 10;
pub(crate) const SHIM_EXIT: u32 = 11;

/// The shim method id for `name` under `flavor` (the union surface, flavor-gated where the shimmed
/// APIs differ: readfrom/readfrom_mem/writeto_mem are MicroPython's, writeto_then_readfrom +
/// the lock protocol CircuitPython's).
pub(crate) fn i2c_shim_method_id(flavor: u32, name: &str) -> Option<u32> {
    match name {
        "scan" => Some(SHIM_SCAN),
        "readfrom" if flavor == SHIM_FLAVOR_MACHINE => Some(SHIM_READFROM),
        "writeto" => Some(SHIM_WRITETO),
        "readfrom_into" => Some(SHIM_READFROM_INTO),
        "readfrom_mem" if flavor == SHIM_FLAVOR_MACHINE => Some(SHIM_READFROM_MEM),
        "writeto_mem" if flavor == SHIM_FLAVOR_MACHINE => Some(SHIM_WRITETO_MEM),
        "writeto_then_readfrom" if flavor == SHIM_FLAVOR_BUSIO => Some(SHIM_WRITETO_THEN_READFROM),
        "try_lock" if flavor == SHIM_FLAVOR_BUSIO => Some(SHIM_TRY_LOCK),
        "unlock" if flavor == SHIM_FLAVOR_BUSIO => Some(SHIM_UNLOCK),
        "deinit" => Some(SHIM_DEINIT),
        "__enter__" if flavor == SHIM_FLAVOR_BUSIO => Some(SHIM_ENTER),
        "__exit__" if flavor == SHIM_FLAVOR_BUSIO => Some(SHIM_EXIT),
        _ => None,
    }
}

impl ObjectModel {
    /// A fresh shim I2C factory carrying its flavor (`machine.I2C` / `busio.I2C`).
    pub(crate) fn i2c_shim_factory(&mut self, flavor: u32) -> Result<Value, Trap> {
        self.alloc_leaf(self.i2c_shim_factory_type_id, &[flavor])
    }

    /// Whether `value` is a shim I2C factory (a callable).
    #[must_use]
    pub(crate) fn is_i2c_shim_factory(&self, value: Value) -> bool {
        self.is_type(value, self.i2c_shim_factory_type_id)
    }

    /// Whether `value` is a shim I2C instance.
    #[must_use]
    pub(crate) fn is_i2c_shim(&self, value: Value) -> bool {
        self.is_type(value, self.i2c_shim_type_id)
    }

    /// The standard `I2cBus` a shim instance wraps.
    fn i2c_shim_bus(&self, shim: Value) -> Value {
        Value::from_bits(self.leaf_word(shim, SHIM_W_BUS))
    }

    /// The flavor of a shim instance.
    pub(crate) fn i2c_shim_flavor(&self, shim: Value) -> u32 {
        self.leaf_word(shim, SHIM_W_FLAVOR)
    }

    /// Constructs a shim I2C: translates the shimmed constructor onto `i2c.open` (machine's 400 kHz
    /// default becomes a frequency translation; busio validates the pins against the table).
    pub(crate) fn call_i2c_shim_factory(
        &mut self,
        factory: Value,
        posargs: &[Value],
        kwargs: &[(&str, Value)],
    ) -> Result<Value, Trap> {
        let flavor = self.leaf_word(factory, 0);
        let mut instance = 0u32;
        let (mut scl, mut sda) = (Value::NONE, Value::NONE);
        let mut frequency = if flavor == SHIM_FLAVOR_MACHINE { 400_000u32 } else { 100_000 };
        if flavor == SHIM_FLAVOR_MACHINE {
            match posargs {
                [id] => instance = self.i2c_shim_bus_id(*id)?,
                _ => {
                    let message = "I2C(id, ...) takes the bus id as its one positional argument";
                    return Err(self.with_message(Trap::TypeError, message));
                }
            }
        } else {
            match posargs {
                [] => {}
                [scl_pin] => scl = *scl_pin,
                [scl_pin, sda_pin] => {
                    scl = *scl_pin;
                    sda = *sda_pin;
                }
                _ => return Err(Trap::TypeError),
            }
        }
        for &(name, value) in kwargs {
            match name {
                "freq" | "frequency" => frequency = self.i2c_shim_frequency(value)?,
                "scl" => scl = value,
                "sda" => sda = value,
                other => {
                    let message =
                        alloc::format!("I2C() got an unexpected keyword argument '{other}'");
                    return Err(self.raise_named_exception("TypeError", &message));
                }
            }
        }
        if let Some((scl_pin, sda_pin)) = self.board().i2c_pins(instance) {
            self.shim_require_pin(scl, scl_pin, "scl")?;
            self.shim_require_pin(sda, sda_pin, "sda")?;
        }
        let resource = self.new_i2c_resource(instance)?;
        let freq = Value::fixnum(frequency as i32).ok_or(Trap::Overflow)?;
        let bus = self.i2c_open(&[resource], &[("frequency", freq)])?;
        self.alloc_leaf(self.i2c_shim_type_id, &[bus.bits(), flavor])
    }

    fn i2c_shim_bus_id(&mut self, value: Value) -> Result<u32, Trap> {
        value
            .as_int()
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| self.with_message(Trap::ValueError, "I2C id must be a small int"))
    }

    fn i2c_shim_frequency(&mut self, value: Value) -> Result<u32, Trap> {
        match value.as_int() {
            Some(f) if f > 0 && f <= i64::from(u32::MAX) => Ok(f as u32),
            _ => Err(self.with_message(Trap::ValueError, "frequency must be a positive integer")),
        }
    }

    /// Rejects a shimmed-verb keyword that is only supportable at its default (the buffer slice /
    /// stop-bit niceties beyond the standard's full-buffer verbs).
    fn i2c_shim_reject_slice(&mut self, kwargs: &[(&str, Value)]) -> Result<(), Trap> {
        for &(name, value) in kwargs {
            let ok = match name {
                "start" => value.as_int() == Some(0),
                "end" => value.is_none(),
                "stop" => value == Value::TRUE,
                "addrsize" => value.as_int() == Some(8),
                _ => false,
            };
            if !ok {
                let message = alloc::format!(
                    "the shim does not support '{name}' here yet (use the standard i2c surface)"
                );
                return Err(self.with_message(Trap::ValueError, &message));
            }
        }
        Ok(())
    }

    /// Dispatches a shim I2C method: each translates onto the standard `I2cBus` dispatch.
    pub(crate) fn call_i2c_shim_method(
        &mut self,
        shim: Value,
        method_id: u32,
        posargs: &[Value],
        kwargs: &[(&str, Value)],
    ) -> Result<Value, Trap> {
        let bus = self.i2c_shim_bus(shim);
        self.i2c_shim_reject_slice(kwargs)?;
        match method_id {
            SHIM_SCAN => self.i2c_bus_dispatch(bus, i2c::BUS_SCAN, &[], Value::NONE),
            SHIM_READFROM => {
                let [addr, n] = posargs else {
                    return Err(Trap::TypeError);
                };
                self.i2c_bus_dispatch(bus, i2c::BUS_READ, &[*addr, *n], Value::NONE)
            }
            SHIM_WRITETO => {
                let [addr, buf] = posargs else {
                    return Err(Trap::TypeError);
                };
                self.i2c_bus_dispatch(bus, i2c::BUS_WRITE, &[*addr, *buf], Value::NONE)
            }
            SHIM_READFROM_INTO => {
                let [addr, buf] = posargs else {
                    return Err(Trap::TypeError);
                };
                let len = self.bytes_value(*buf).map_or(0, <[u8]>::len);
                let n = Value::fixnum(len as i32).ok_or(Trap::Overflow)?;
                let result = self.i2c_bus_dispatch(bus, i2c::BUS_READ, &[*addr, n], Value::NONE)?;
                let data = self.bytes_value(result).map(<[u8]>::to_vec).unwrap_or_default();
                self.fill_bytearray_prefix(*buf, &data)?;
                Ok(Value::NONE)
            }
            SHIM_READFROM_MEM => {
                let [addr, memaddr, n] = posargs else {
                    return Err(Trap::TypeError);
                };
                self.i2c_bus_dispatch(bus, i2c::BUS_READ_REGISTER, &[*addr, *memaddr, *n], Value::NONE)
            }
            SHIM_WRITETO_MEM => {
                let [addr, memaddr, buf] = posargs else {
                    return Err(Trap::TypeError);
                };
                self.i2c_bus_dispatch(
                    bus,
                    i2c::BUS_WRITE_REGISTER,
                    &[*addr, *memaddr, *buf],
                    Value::NONE,
                )
            }
            SHIM_WRITETO_THEN_READFROM => {
                let [addr, out, in_buf] = posargs else {
                    return Err(Trap::TypeError);
                };
                let len = self.bytes_value(*in_buf).map_or(0, <[u8]>::len);
                let n = Value::fixnum(len as i32).ok_or(Trap::Overflow)?;
                let result = self.i2c_bus_dispatch(
                    bus,
                    i2c::BUS_WRITE_THEN_READ,
                    &[*addr, *out, n],
                    Value::NONE,
                )?;
                let data = self.bytes_value(result).map(<[u8]>::to_vec).unwrap_or_default();
                self.fill_bytearray_prefix(*in_buf, &data)?;
                Ok(Value::NONE)
            }
            SHIM_TRY_LOCK => Ok(Value::TRUE),
            SHIM_UNLOCK => Ok(Value::NONE),
            SHIM_DEINIT | SHIM_EXIT => {
                self.i2c_bus_dispatch(bus, i2c::BUS_CLOSE, &[], Value::NONE)?;
                Ok(Value::NONE)
            }
            SHIM_ENTER => Ok(shim),
            _ => Err(Trap::AttributeError),
        }
    }
}
