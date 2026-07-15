//! The MicroPython `machine.ADC` / CircuitPython `analogio.AnalogIn` shim faces -- thin wrappers
//! over the Lamella-standard `adc.open` + `Channel`. The flavor gates the surface: machine exposes
//! read_u16()/read_uv() METHODS (the convergence dividend, 1:1 with the standard); analogio exposes
//! a `.value` PROPERTY whose READ performs a conversion + a float `.reference_voltage` -- both
//! side-effecting-property quirks live here (in the shim's getattr), never in the standard surface.

use crate::adc;
use crate::object::ObjectModel;
use crate::trap::Trap;
use crate::value::Value;

/// Which shimmed API a factory/instance wears.
pub(crate) const SHIM_FLAVOR_MACHINE: u32 = 0;
pub(crate) const SHIM_FLAVOR_ANALOGIO: u32 = 1;

pub(crate) const SHIM_W_CHANNEL: u32 = 0;
pub(crate) const SHIM_W_FLAVOR: u32 = 1;
pub(crate) const SHIM_WORDS: u32 = 2;

pub(crate) const SHIM_READ_U16: u32 = 0;
pub(crate) const SHIM_READ_UV: u32 = 1;
pub(crate) const SHIM_INIT: u32 = 2;
pub(crate) const SHIM_DEINIT: u32 = 3;
pub(crate) const SHIM_ENTER: u32 = 4;
pub(crate) const SHIM_EXIT: u32 = 5;

/// The shim method id for `name` under `flavor` (read_u16/read_uv/init are MicroPython's; analogio's
/// value/reference_voltage are PROPERTIES resolved in the getattr, not methods).
pub(crate) fn adc_shim_method_id(flavor: u32, name: &str) -> Option<u32> {
    match name {
        "read_u16" if flavor == SHIM_FLAVOR_MACHINE => Some(SHIM_READ_U16),
        "read_uv" if flavor == SHIM_FLAVOR_MACHINE => Some(SHIM_READ_UV),
        "init" if flavor == SHIM_FLAVOR_MACHINE => Some(SHIM_INIT),
        "deinit" => Some(SHIM_DEINIT),
        "__enter__" if flavor == SHIM_FLAVOR_ANALOGIO => Some(SHIM_ENTER),
        "__exit__" if flavor == SHIM_FLAVOR_ANALOGIO => Some(SHIM_EXIT),
        _ => None,
    }
}

impl ObjectModel {
    /// A fresh shim ADC factory carrying its flavor (`machine.ADC` / `analogio.AnalogIn`).
    pub(crate) fn adc_shim_factory(&mut self, flavor: u32) -> Result<Value, Trap> {
        self.alloc_leaf(self.adc_shim_factory_type_id, &[flavor])
    }

    /// Whether `value` is a shim ADC factory (a callable).
    #[must_use]
    pub(crate) fn is_adc_shim_factory(&self, value: Value) -> bool {
        self.is_type(value, self.adc_shim_factory_type_id)
    }

    /// Whether `value` is a shim ADC instance.
    #[must_use]
    pub(crate) fn is_adc_shim(&self, value: Value) -> bool {
        self.is_type(value, self.adc_shim_type_id)
    }

    /// The standard `Channel` a shim instance wraps.
    pub(crate) fn adc_shim_channel(&self, shim: Value) -> Value {
        Value::from_bits(self.leaf_word(shim, SHIM_W_CHANNEL))
    }

    /// The flavor of a shim instance.
    pub(crate) fn adc_shim_flavor(&self, shim: Value) -> u32 {
        self.leaf_word(shim, SHIM_W_FLAVOR)
    }

    /// Constructs a shim ADC: opens the standard channel for a board ADC resource (`board.A0`) or a
    /// channel number, and wraps it.
    pub(crate) fn call_adc_shim_factory(
        &mut self,
        factory: Value,
        posargs: &[Value],
    ) -> Result<Value, Trap> {
        let flavor = self.leaf_word(factory, 0);
        let [arg] = posargs else {
            let message = "ADC(pin) / AnalogIn(pin) takes exactly one argument";
            return Err(self.with_message(Trap::TypeError, message));
        };
        let resource = if self.is_adc_resource(*arg) {
            *arg
        } else if let Some(channel) = arg.as_int().and_then(|n| u32::try_from(n).ok()) {
            match self.board().adc_channel_source(channel) {
                Some(pin) => self.new_adc_resource(channel, pin)?,
                None => {
                    return Err(self.with_message(Trap::ValueError, "ADC channel out of range"));
                }
            }
        } else {
            let message = "pass board.A0 / board.TEMP_SENSOR, or a channel number";
            return Err(self.with_message(Trap::TypeError, message));
        };
        let channel = self.adc_open(&[resource])?;
        self.alloc_leaf(self.adc_shim_type_id, &[channel.bits(), flavor])
    }

    /// Dispatches a shim ADC method: read_u16/read_uv translate 1:1 onto the standard channel;
    /// init() loud-rejects (the table declares no sampling knobs yet); deinit closes.
    pub(crate) fn call_adc_shim_method(
        &mut self,
        shim: Value,
        method_id: u32,
        args: &[Value],
    ) -> Result<Value, Trap> {
        let channel = self.adc_shim_channel(shim);
        match method_id {
            SHIM_READ_U16 => self.call_adc_channel_method(channel, adc::CH_READ_U16, args),
            SHIM_READ_UV => self.call_adc_channel_method(channel, adc::CH_READ_UV, args),
            SHIM_INIT => {
                let message =
                    "ADC.init(sample_ns=, atten=) is not supported (the table declares no sampling knobs yet)";
                Err(self.with_message(Trap::ValueError, message))
            }
            SHIM_DEINIT | SHIM_EXIT => {
                self.call_adc_channel_method(channel, adc::CH_CLOSE, &[])?;
                Ok(Value::NONE)
            }
            SHIM_ENTER => Ok(shim),
            _ => Err(Trap::AttributeError),
        }
    }
}
