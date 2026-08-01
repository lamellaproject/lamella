//! The board binding: resolving a role handle through the board's GENERATED `board.py`.

use crate::object::ObjectModel;
use crate::tables::uart_samd21::Samd21UartFacts;
use crate::trap::Trap;
use crate::value::Value;
use alloc::format;

/// The module an app imports to name its board's peripherals.
const BOARD_MODULE: &str = "board";

/// The `FACTS[role]` descriptor from the imported `board` module.
///
/// Each miss is loud and distinct, because each one is a different repair: no `board` module means
/// the deploy layer presented none (or the app never imported it), no `FACTS` means the module is
/// not the generated one, and an unknown role means the app named a handle this board does not
/// carry.
pub(crate) fn role_facts(model: &mut ObjectModel, role: &str) -> Result<Value, Trap> {
    let module = match model.import_builtin_module(BOARD_MODULE) {
        Some(Ok(module)) => module,
        Some(Err(trap)) => return Err(trap),
        None => {
            let message = "no board module -- `import board` first (the deploy layer presents the \
                           target board's generated module)";
            return Err(model.with_message(Trap::ValueError, message));
        }
    };
    let namespace = model.module_namespace(module);
    let Some(facts) = model.dict_get_str(namespace, "FACTS") else {
        let message = "the board module carries no FACTS -- expected the generated \
                       bsp/<board>/python/board.py";
        return Err(model.with_message(Trap::ValueError, message));
    };
    let Some(descriptor) = model.dict_get_str(facts, role) else {
        let message = format!("this board carries no '{role}' role");
        return Err(model.raise_named_exception("ValueError", &message));
    };
    Ok(descriptor)
}

/// One resolved unsigned fact of `role`, by key.
///
/// Reads the whole int lane (a register address like `0x42001400` exceeds the fixnum range, so a
/// generated fact routinely arrives as a heap `long`); anything that is not an int in `u32` range
/// is a malformed descriptor, and says so.
pub(crate) fn fact_u32(
    model: &mut ObjectModel,
    descriptor: Value,
    role: &str,
    key: &str,
) -> Result<u32, Trap> {
    let Some(value) = model.dict_get_str(descriptor, key) else {
        let message = format!("the '{role}' facts carry no '{key}'");
        return Err(model.raise_named_exception("ValueError", &message));
    };
    let Some(int) = model.as_i128(value) else {
        let message = format!("the '{role}' fact '{key}' is not an integer");
        return Err(model.raise_named_exception("ValueError", &message));
    };
    u32::try_from(int).map_err(|_| {
        let message = format!("the '{role}' fact '{key}' does not fit a 32-bit register value");
        model.raise_named_exception("ValueError", &message)
    })
}

/// Asserts `role` names a peripheral of `kind`, so a handle opened by the wrong surface fails at
/// the seam instead of programming registers that belong to another peripheral.
fn require_kind(
    model: &mut ObjectModel,
    descriptor: Value,
    role: &str,
    kind: &str,
) -> Result<(), Trap> {
    let actual = model
        .dict_get_str(descriptor, "kind")
        .and_then(|value| model.str_value(value))
        .map(alloc::string::ToString::to_string);
    match actual {
        Some(ref found) if found == kind => Ok(()),
        Some(found) => {
            let message = format!("board role '{role}' is a {found}, not a {kind}");
            Err(model.raise_named_exception("ValueError", &message))
        }
        None => {
            let message = format!("the '{role}' facts carry no 'kind'");
            Err(model.raise_named_exception("ValueError", &message))
        }
    }
}

/// The SAMD21 SERCOM USART facts for `role`, resolved from the generated module.
///
/// The field names are the generated descriptor's keys, which are the board's C# binding consts
/// under the one renaming rule -- so this list is the whole contract between the emitter and the
/// driver, and a rename on either side fails here rather than programming a wrong register.
pub(crate) fn samd21_uart_facts(
    model: &mut ObjectModel,
    role: &str,
) -> Result<Samd21UartFacts, Trap> {
    let descriptor = role_facts(model, role)?;
    require_kind(model, descriptor, role, "uart")?;
    Ok(Samd21UartFacts {
        sercom_base: fact_u32(model, descriptor, role, "sercom_base")?,
        gclk_clkctrl_value: fact_u32(model, descriptor, role, "gclk_clkctrl_value")?,
        apbc_mask: fact_u32(model, descriptor, role, "apbc_mask")?,
        pmux_reg: fact_u32(model, descriptor, role, "pmux_reg")?,
        pmux_pair: fact_u32(model, descriptor, role, "pmux_pair")?,
        pincfg_tx_reg: fact_u32(model, descriptor, role, "pincfg_tx_reg")?,
        pincfg_rx_reg: fact_u32(model, descriptor, role, "pincfg_rx_reg")?,
        txpo: fact_u32(model, descriptor, role, "txpo")?,
        rxpo: fact_u32(model, descriptor, role, "rxpo")?,
        baud_115200: fact_u32(model, descriptor, role, "baud_115200_osc8m_8mhz")?,
    })
}
