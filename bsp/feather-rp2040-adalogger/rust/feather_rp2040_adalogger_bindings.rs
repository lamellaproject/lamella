
pub const BOARD_MODEL: u16 = 18;

pub const UART0_BASE: u32 = 0x40034000;
pub const UART0_RESET_MASK: u32 = 0x400120;
pub const UART0_IO_TX_CTRL: u32 = 0x40014004;
pub const UART0_IO_RX_CTRL: u32 = 0x4001400C;
pub const UART0_FUNCSEL: u32 = 2;
pub const UART0_CLK_PERI_HZ: u32 = 12000000;

pub const LED_PORT_BASE: u32 = 0xD0000000;
pub const LED_PIN: u32 = 13;
pub const LED_MASK: u32 = 0x2000;
pub const LED_ACTIVE_LOW: u32 = 0;
pub const NEOPIXEL_PORT_BASE: u32 = 0xD0000000;
pub const NEOPIXEL_PIN: u32 = 17;
pub const NEOPIXEL_MASK: u32 = 0x20000;
pub const NEOPIXEL_ACTIVE_LOW: u32 = 0;
pub const BUTTON_PORT_BASE: u32 = 0xD0000000;
pub const BUTTON_PIN: u32 = 7;
pub const BUTTON_MASK: u32 = 0x80;
pub const BUTTON_ACTIVE_LOW: u32 = 1;
pub const SD_CARD_DETECT_PORT_BASE: u32 = 0xD0000000;
pub const SD_CARD_DETECT_PIN: u32 = 16;
pub const SD_CARD_DETECT_MASK: u32 = 0x10000;
pub const SD_CARD_DETECT_ACTIVE_LOW: u32 = 1;
