
pub const BOARD_MODEL: u16 = 7;

pub const VCP_BASE: u32 = 0x40004400;
pub const VCP_RCC_EN_REG: u32 = 0x4002101C;
pub const VCP_RCC_EN_MASK: u32 = 0x20000;
pub const VCP_PORT_RCC_EN_REG: u32 = 0x40021014;
pub const VCP_PORT_RCC_EN_MASK: u32 = 0x20000;
pub const VCP_MODER_REG: u32 = 0x48000000;
pub const VCP_MODER_MASK: u32 = 0xF0;
pub const VCP_MODER_VALUE: u32 = 0xA0;
pub const VCP_AFRL_REG: u32 = 0x48000020;
pub const VCP_AFRL_MASK: u32 = 0xFF00;
pub const VCP_AFRL_VALUE: u32 = 0x1100;
pub const VCP_PCLK1_HZ: u32 = 8000000;
pub const VCP_BRR_115200_HSI_8MHZ: u32 = 0x45;
