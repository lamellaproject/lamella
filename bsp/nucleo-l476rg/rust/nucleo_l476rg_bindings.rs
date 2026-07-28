
pub const BOARD_MODEL: u16 = 8;

pub const VCP_BASE: u32 = 0x40004400;
pub const VCP_RCC_EN_REG: u32 = 0x40021058;
pub const VCP_RCC_EN_MASK: u32 = 0x20000;
pub const VCP_PORT_RCC_EN_REG: u32 = 0x4002104C;
pub const VCP_PORT_RCC_EN_MASK: u32 = 0x1;
pub const VCP_MODER_REG: u32 = 0x48000000;
pub const VCP_MODER_MASK: u32 = 0xF0;
pub const VCP_MODER_VALUE: u32 = 0xA0;
pub const VCP_AFRL_REG: u32 = 0x48000020;
pub const VCP_AFRL_MASK: u32 = 0xFF00;
pub const VCP_AFRL_VALUE: u32 = 0x7700;
pub const VCP_PCLK1_HZ: u32 = 4000000;
pub const VCP_BRR_115200_MSI_4MHZ: u32 = 0x23;
