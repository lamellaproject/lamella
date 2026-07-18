
pub const BOARD_MODEL: u16 = 15;

pub const VCP_BASE: u32 = 0x400E0800;
pub const VCP_PID: u32 = 8;
pub const VCP_PMC_PCER_REG: u32 = 0x400E0610;
pub const VCP_PMC_PCER_MASK: u32 = 0x100;
pub const VCP_PIO_PDR_REG: u32 = 0x400E0E04;
pub const VCP_PIO_ABSR_REG: u32 = 0x400E0E70;
pub const VCP_PIO_MASK: u32 = 0x300;
pub const VCP_PIO_FUNC: u32 = 0;
pub const VCP_MCK_HZ: u32 = 84000000;
pub const VCP_BRGR_CD_115200_PLLA_84MHZ: u32 = 46;
