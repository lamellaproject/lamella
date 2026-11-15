//! The Adafruit Metro RP2350 with PSRAM: an RP2350B with a 12 MHz crystal, 16 MB of QSPI flash and 8 MB
//! of QSPI PSRAM, its select on GP47. Its console is UART0, transmitting on GP0.

use crate::rp2350;

#[allow(dead_code)]
#[path = "../../../../bsp/boards/adafruit-metro-rp2350-psram/rust/adafruit_metro_rp2350_psram_bindings.rs"]
mod bindings;

/// The board as its generated bindings state it: the console on the `uart0` carrier at 115200 baud
/// under the default plan, and that plan.
/// No document of the board's maker prints its PSRAM's part, and the part's commands and timing are
/// what the bring-up is told, so this board brings up no PSRAM and runs from its flash and the
/// chip's SRAM, as its facts allow.
pub const BOARD: rp2350::Board = rp2350::Board {
    console: rp2350::Console {
        uart_base: bindings::UART0_BASE,
        release_mask: bindings::UART0_RESET_MASK,
        io_tx_ctrl: bindings::UART0_IO_TX_CTRL,
        pads_tx: bindings::UART0_PADS_TX,
        funcsel: bindings::UART0_FUNCSEL,
        ibrd: bindings::UART0_IBRD_115200_PLL_150_48,
        fbrd: bindings::UART0_FBRD_115200_PLL_150_48,
        clk_peri_hz: bindings::UART0_CLK_PERI_HZ,
    },
    plan: rp2350::Plan {
        xosc_hz: bindings::XOSC_HZ_PLL_150_48,
        pll_sys_fbdiv: bindings::PLL_SYS_FBDIV_PLL_150_48,
        pll_sys_prim: bindings::PLL_SYS_PRIM_PLL_150_48,
        pll_usb_fbdiv: bindings::PLL_USB_FBDIV_PLL_150_48,
        pll_usb_prim: bindings::PLL_USB_PRIM_PLL_150_48,
    },
    psram: None,
}
.checked();
