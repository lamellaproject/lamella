
pub const BOARD_MODEL: u16 = 9;

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
