//! The MicroPython `machine.SPI` / CircuitPython `busio.SPI` shim faces -- thin wrappers over the
//! Lamella-standard `spi.open` + `SpiBus`, exactly as `machine.Pin` is over `gpio`. Neither shim
//! manages chip-select (that is the driver's job, per shim contracts), so both open a
//! RAW bus. The flavor gates the visible names and holds each shimmed API's quirks (machine's
//! read-returns-bytes + firstbit/polarity/phase spelling; busio's lock protocol + configure-as-
//! reconfigure) OUTSIDE the standard surface. The dispatch glue lives here, in the shim's own file.

use crate::object::ObjectModel;
use crate::spi::{self, SpiConfig};
use crate::trap::Trap;
use crate::value::Value;

/// Which shimmed API a factory/instance wears.
pub(crate) const SHIM_FLAVOR_MACHINE: u32 = 0;
pub(crate) const SHIM_FLAVOR_BUSIO: u32 = 1;

pub(crate) const SHIM_W_BUS: u32 = 0;
pub(crate) const SHIM_W_FLAVOR: u32 = 1;
pub(crate) const SHIM_WORDS: u32 = 2;

pub(crate) const SHIM_READ: u32 = 0;
pub(crate) const SHIM_READINTO: u32 = 1;
pub(crate) const SHIM_WRITE: u32 = 2;
pub(crate) const SHIM_WRITE_READINTO: u32 = 3;
pub(crate) const SHIM_CONFIGURE: u32 = 4;
pub(crate) const SHIM_TRY_LOCK: u32 = 5;
pub(crate) const SHIM_UNLOCK: u32 = 6;
pub(crate) const SHIM_DEINIT: u32 = 7;
pub(crate) const SHIM_ENTER: u32 = 8;
pub(crate) const SHIM_EXIT: u32 = 9;

/// The shim method id for `name` under `flavor` (the union surface, flavor-gated where the shimmed
/// APIs differ: `read` is MicroPython's, `configure`/`try_lock`/`unlock` CircuitPython's).
pub(crate) fn spi_shim_method_id(flavor: u32, name: &str) -> Option<u32> {
    match name {
        "read" if flavor == SHIM_FLAVOR_MACHINE => Some(SHIM_READ),
        "readinto" => Some(SHIM_READINTO),
        "write" => Some(SHIM_WRITE),
        "write_readinto" => Some(SHIM_WRITE_READINTO),
        "configure" if flavor == SHIM_FLAVOR_BUSIO => Some(SHIM_CONFIGURE),
        "try_lock" if flavor == SHIM_FLAVOR_BUSIO => Some(SHIM_TRY_LOCK),
        "unlock" if flavor == SHIM_FLAVOR_BUSIO => Some(SHIM_UNLOCK),
        "deinit" => Some(SHIM_DEINIT),
        "__enter__" if flavor == SHIM_FLAVOR_BUSIO => Some(SHIM_ENTER),
        "__exit__" if flavor == SHIM_FLAVOR_BUSIO => Some(SHIM_EXIT),
        _ => None,
    }
}

impl ObjectModel {
    /// A fresh shim SPI factory carrying its flavor (`machine.SPI` / `busio.SPI`).
    pub(crate) fn spi_shim_factory(&mut self, flavor: u32) -> Result<Value, Trap> {
        self.alloc_leaf(self.spi_shim_factory_type_id, &[flavor])
    }

    /// Whether `value` is a shim SPI factory (a callable).
    #[must_use]
    pub(crate) fn is_spi_shim_factory(&self, value: Value) -> bool {
        self.is_type(value, self.spi_shim_factory_type_id)
    }

    /// Whether `value` is a shim SPI instance.
    #[must_use]
    pub(crate) fn is_spi_shim(&self, value: Value) -> bool {
        self.is_type(value, self.spi_shim_type_id)
    }

    /// The standard `SpiBus` a shim instance wraps.
    fn spi_shim_bus(&self, shim: Value) -> Value {
        Value::from_bits(self.leaf_word(shim, SHIM_W_BUS))
    }

    /// The flavor of a shim instance.
    pub(crate) fn spi_shim_flavor(&self, shim: Value) -> u32 {
        self.leaf_word(shim, SHIM_W_FLAVOR)
    }

    /// Constructs a shim SPI: translates the shimmed constructor onto `spi.open` (a raw bus -- CS is
    /// the driver's), holding no extra state (the bus carries the config).
    pub(crate) fn call_spi_shim_factory(
        &mut self,
        factory: Value,
        posargs: &[Value],
        kwargs: &[(&str, Value)],
    ) -> Result<Value, Trap> {
        let flavor = self.leaf_word(factory, 0);
        let mut config = SpiConfig::default();
        let mut polarity = 0u32;
        let mut phase = 0u32;
        let mut instance = 0u32;
        let (mut sck, mut mosi, mut miso) = (Value::NONE, Value::NONE, Value::NONE);
        if flavor == SHIM_FLAVOR_MACHINE {
            match posargs {
                [id] => instance = self.spi_shim_bus_id(*id)?,
                [id, baudrate] => {
                    instance = self.spi_shim_bus_id(*id)?;
                    config.frequency = self.spi_shim_baudrate(*baudrate)?;
                }
                _ => {
                    let message = "SPI(id[, baudrate], ...) takes the bus id as its first argument";
                    return Err(self.with_message(Trap::TypeError, message));
                }
            }
        } else {
            match posargs {
                [] => {}
                [clock] => sck = *clock,
                [clock, m_out] => {
                    sck = *clock;
                    mosi = *m_out;
                }
                [clock, m_out, m_in] => {
                    sck = *clock;
                    mosi = *m_out;
                    miso = *m_in;
                }
                _ => return Err(Trap::TypeError),
            }
        }
        for &(name, value) in kwargs {
            match name {
                "baudrate" => config.frequency = self.spi_shim_baudrate(value)?,
                "polarity" => polarity = self.spi_shim_bit(value, "polarity")?,
                "phase" => phase = self.spi_shim_bit(value, "phase")?,
                "bits" => {
                    if value.as_int() != Some(8) {
                        return Err(self.with_message(Trap::ValueError, "bits must be 8"));
                    }
                }
                "firstbit" if flavor == SHIM_FLAVOR_MACHINE => {
                    config.bit_order = if value.as_int() == Some(1) {
                        spi::BIT_ORDER_LSB
                    } else {
                        spi::BIT_ORDER_MSB
                    };
                }
                "sck" | "clock" => sck = value,
                "mosi" | "MOSI" => mosi = value,
                "miso" | "MISO" => miso = value,
                other => {
                    let message =
                        alloc::format!("SPI() got an unexpected keyword argument '{other}'");
                    return Err(self.raise_named_exception("TypeError", &message));
                }
            }
        }
        config.mode = (polarity << 1) | phase;
        if let Some((sck_pin, mosi_pin, miso_pin)) = self.board().spi_pins(instance) {
            self.shim_require_pin(sck, sck_pin, "clock")?;
            self.shim_require_pin(mosi, mosi_pin, "MOSI")?;
            self.shim_require_pin(miso, miso_pin, "MISO")?;
        }
        let resource = self.new_spi_resource(instance)?;
        let baud = Value::fixnum(config.frequency as i32).ok_or(Trap::Overflow)?;
        let mode = Value::fixnum(config.mode as i32).ok_or(Trap::Overflow)?;
        let bit_order = self.new_str(spi::bit_order_name(config.bit_order))?;
        let pairs = [("frequency", baud), ("mode", mode), ("bit_order", bit_order)];
        let bus = self.spi_open(&[resource], &pairs)?;
        self.alloc_leaf(self.spi_shim_type_id, &[bus.bits(), flavor])
    }

    fn spi_shim_bus_id(&mut self, value: Value) -> Result<u32, Trap> {
        value
            .as_int()
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| self.with_message(Trap::ValueError, "SPI id must be a small int"))
    }

    fn spi_shim_baudrate(&mut self, value: Value) -> Result<u32, Trap> {
        match value.as_int() {
            Some(b) if b > 0 && b <= i64::from(u32::MAX) => Ok(b as u32),
            _ => Err(self.with_message(Trap::ValueError, "baudrate must be a positive integer")),
        }
    }

    fn spi_shim_bit(&mut self, value: Value, which: &str) -> Result<u32, Trap> {
        match value.as_int() {
            Some(0) => Ok(0),
            Some(1) => Ok(1),
            _ => {
                let message = alloc::format!("{which} must be 0 or 1");
                Err(self.with_message(Trap::ValueError, &message))
            }
        }
    }

    /// Dispatches a shim SPI method: each translates onto the standard `SpiBus` dispatch, re-wrapping
    /// the result in the shimmed API's convention (`None` where the standard returns a count).
    pub(crate) fn call_spi_shim_method(
        &mut self,
        shim: Value,
        method_id: u32,
        posargs: &[Value],
        kwargs: &[(&str, Value)],
    ) -> Result<Value, Trap> {
        let bus = self.spi_shim_bus(shim);
        match method_id {
            SHIM_READ => {
                let (n, fill) = match posargs {
                    [n] => (*n, self.shim_write_value(kwargs)?),
                    [n, write] => (*n, *write),
                    _ => return Err(Trap::TypeError),
                };
                self.spi_bus_dispatch(bus, spi::BUS_READ, &[n], fill)
            }
            SHIM_READINTO => {
                let (buf, fill) = match posargs {
                    [buf] => (*buf, self.shim_write_value(kwargs)?),
                    [buf, write] => (*buf, *write),
                    _ => return Err(Trap::TypeError),
                };
                let len = self.bytes_value(buf).map_or(0, <[u8]>::len);
                let fill_byte = fill.as_int().unwrap_or(0) as u8;
                let out = self.new_bytes(alloc::vec![fill_byte; len])?;
                self.spi_bus_dispatch(bus, spi::BUS_TRANSFER_INTO, &[out, buf], Value::NONE)?;
                Ok(Value::NONE)
            }
            SHIM_WRITE => {
                let [buf] = posargs else {
                    return Err(Trap::TypeError);
                };
                self.spi_bus_dispatch(bus, spi::BUS_WRITE, &[*buf], Value::NONE)?;
                Ok(Value::NONE)
            }
            SHIM_WRITE_READINTO => {
                let [wbuf, rbuf] = posargs else {
                    return Err(Trap::TypeError);
                };
                self.spi_bus_dispatch(bus, spi::BUS_TRANSFER_INTO, &[*wbuf, *rbuf], Value::NONE)?;
                Ok(Value::NONE)
            }
            SHIM_CONFIGURE => {
                self.spi_shim_configure(bus, posargs, kwargs)
            }
            SHIM_TRY_LOCK => Ok(Value::TRUE),
            SHIM_UNLOCK => Ok(Value::NONE),
            SHIM_DEINIT | SHIM_EXIT => {
                self.spi_bus_dispatch(bus, spi::BUS_CLOSE, &[], Value::NONE)?;
                Ok(Value::NONE)
            }
            SHIM_ENTER => Ok(shim),
            _ => Err(Trap::AttributeError),
        }
    }

    /// The `write=`/`write_value=` fill byte from a shim read's keywords (default 0x00).
    fn shim_write_value(&mut self, kwargs: &[(&str, Value)]) -> Result<Value, Trap> {
        let mut fill = Value::NONE;
        for &(name, value) in kwargs {
            if name == "write" || name == "write_value" {
                fill = value;
            } else {
                let message =
                    alloc::format!("this method got an unexpected keyword argument '{name}'");
                return Err(self.raise_named_exception("TypeError", &message));
            }
        }
        Ok(fill)
    }

    /// `busio.SPI.configure`: a configure() equal to the current config is a no-op (the per-
    /// transaction common case); a real change reconfigures WITHOUT releasing the claim.
    fn spi_shim_configure(
        &mut self,
        bus: Value,
        posargs: &[Value],
        kwargs: &[(&str, Value)],
    ) -> Result<Value, Trap> {
        use crate::spi::{BUS_W_BIT_ORDER, BUS_W_FREQUENCY, BUS_W_MODE};
        if !posargs.is_empty() {
            return Err(Trap::TypeError);
        }
        let mut config = SpiConfig {
            frequency: self.leaf_word(bus, BUS_W_FREQUENCY),
            mode: self.leaf_word(bus, BUS_W_MODE),
            bit_order: self.leaf_word(bus, BUS_W_BIT_ORDER),
        };
        let (mut polarity, mut phase, mut mode_set) = (0u32, 0u32, false);
        for &(name, value) in kwargs {
            match name {
                "baudrate" => config.frequency = self.spi_shim_baudrate(value)?,
                "polarity" => {
                    polarity = self.spi_shim_bit(value, "polarity")?;
                    mode_set = true;
                }
                "phase" => {
                    phase = self.spi_shim_bit(value, "phase")?;
                    mode_set = true;
                }
                "bits" => {
                    if value.as_int() != Some(8) {
                        return Err(self.with_message(Trap::ValueError, "bits must be 8"));
                    }
                }
                other => {
                    let message =
                        alloc::format!("configure() got an unexpected keyword argument '{other}'");
                    return Err(self.raise_named_exception("TypeError", &message));
                }
            }
        }
        if mode_set {
            config.mode = (polarity << 1) | phase;
        }
        self.spi_reconfigure(bus, &config)?;
        Ok(Value::NONE)
    }
}
