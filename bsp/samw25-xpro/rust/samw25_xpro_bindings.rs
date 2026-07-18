
pub const BOARD_MODEL: u16 = 6;
pub const CARRIER_USB_VID: u16 = 0x03EB;
pub const CARRIER_USB_PID: u16 = 0x2111;

pub const VCP_SERCOM_BASE: u32 = 0x42001800;
pub const VCP_IRQ: u32 = 13;
pub const VCP_GCLK_CLKCTRL_VALUE: u32 = 0x4018;
pub const VCP_APBC_MASK: u32 = 0x40;
pub const VCP_PMUX_REG: u32 = 0x410044B5;
pub const VCP_PMUX_PAIR: u32 = 0x33;
pub const VCP_PINCFG_TX_REG: u32 = 0x410044CA;
pub const VCP_PINCFG_RX_REG: u32 = 0x410044CB;
pub const VCP_TXPO: u32 = 1;
pub const VCP_RXPO: u32 = 3;
pub const VCP_BAUD_115200_OSC8M_8MHZ: u32 = 0xC505;

pub const WINC_RESET_N_PORT_BASE: u32 = 0x41004400;
pub const WINC_RESET_N_PIN: u32 = 27;
pub const WINC_RESET_N_MASK: u32 = 0x8000000;
pub const WINC_RESET_N_ACTIVE_LOW: u32 = 1;
pub const WINC_CHIP_EN_PORT_BASE: u32 = 0x41004400;
pub const WINC_CHIP_EN_PIN: u32 = 28;
pub const WINC_CHIP_EN_MASK: u32 = 0x10000000;
pub const WINC_CHIP_EN_ACTIVE_LOW: u32 = 0;
pub const WINC_IRQN_PORT_BASE: u32 = 0x41004480;
pub const WINC_IRQN_PIN: u32 = 9;
pub const WINC_IRQN_MASK: u32 = 0x200;
pub const WINC_IRQN_ACTIVE_LOW: u32 = 1;
