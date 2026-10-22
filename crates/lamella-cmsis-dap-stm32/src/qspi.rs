//! The quad-SPI NOR flash an STM32F7 board fits outside the chip, reached through the chip's own
//! QUADSPI controller from a debug probe: brought up, identified by its JEDEC identity, erased and
//! programmed by a loader the host places in the chip's SRAM, and read back through the
//! controller's memory-mapped window.
//!
//! The controller is the same block on the STM32F74x/F75x (RM0385 chapter 14) and the STM32F76x/F77x
//! (RM0410 chapter 14): the same base, register offsets and bit positions, checked in both manuals.
//! What differs from board to board is which pins carry its six signals and which flash part is
//! soldered to them, so the pins arrive as a [`QspiWiring`] and the part is recognized by its
//! identity among the [`NorPart`]s a board allows.
//!
//! # The device rules every operation here keeps
//!
//! - **The identity first.** Nothing is erased, programmed or mapped until the part has answered
//!   its JEDEC identity and that identity names a part the caller allows.
//! - **The sample shift before the identity read.** [`bring_up`] sets `SSHIFT`, because without it a
//!   part's answer can arrive late enough to lose a bit.
//! - **A reset before every read or verify.** A part keeps volatile settings (dummy cycles, a
//!   protocol mode) that the program it last served may have changed, and a chip reset does not
//!   clear them. [`reset_part`] sends the part's own reset sequence, so a read uses the part's
//!   power-up behavior and not whatever the last program left.
//! - **The plain read, and single-line writes.** The window is mapped with the part's plain READ
//!   command, which has no dummy and no alternate-byte phase, and erases and page programs use one
//!   line. Neither then depends on the part's dummy-cycle setting or its quad-enable bit.
//! - **Bounded erase calls.** Blocks are erased a few per loader call, so the slowest erase the
//!   datasheet allows fits a call's halt-poll budget many times over.

use lamella_probe_core::{CallFrame, ProbeError, TargetAccess};

// ---- the QUADSPI controller ---------------------------------------------------------------------

/// The QUADSPI register block: RM0385 and RM0410 2.2.2, "QUADSPI Control", `0xA000 1000`.
pub const STM32F7_QUADSPI: u32 = 0xA000_1000;

/// Where the controller maps the flash in memory-mapped mode: RM0385 and RM0410 14.3.7 and the
/// memory map's quad-SPI bank. An access here before the controller is configured is a bus fault.
pub const STM32F7_QUADSPI_WINDOW: u32 = 0x9000_0000;

/// The largest flash the memory-mapped window reaches: RM0410 14.5.2, "the addressable space in
/// memory-mapped mode is limited to 256 Mbytes".
pub const STM32F7_QUADSPI_WINDOW_LIMIT: u32 = 256 * 1024 * 1024;

// Register offsets, RM0385 and RM0410 14.5.1-14.5.13. The address register is the loader's alone.
const QSPI_CR: u32 = 0x00;
const QSPI_DCR: u32 = 0x04;
const QSPI_SR: u32 = 0x08;
const QSPI_FCR: u32 = 0x0C;
const QSPI_DLR: u32 = 0x10;
const QSPI_CCR: u32 = 0x14;
const QSPI_DR: u32 = 0x20;

// QUADSPI_CR (14.5.1): EN bit 0, ABORT bit 1, SSHIFT bit 4, PRESCALER bits 31:24.
const CR_EN: u32 = 1 << 0;
const CR_ABORT: u32 = 1 << 1;
const CR_SSHIFT: u32 = 1 << 4;
const CR_PRESCALER_SHIFT: u32 = 24;

// QUADSPI_DCR (14.5.2): CKMODE bit 0, CSHT bits 10:8, FSIZE bits 20:16.
const DCR_CSHT_SHIFT: u32 = 8;
const DCR_FSIZE_SHIFT: u32 = 16;

// QUADSPI_SR (14.5.3): TEF bit 0, TCF bit 1, BUSY bit 5.
const SR_TEF: u32 = 1 << 0;
const SR_TCF: u32 = 1 << 1;
const SR_BUSY: u32 = 1 << 5;

// QUADSPI_FCR (14.5.4): CTEF bit 0, CTCF bit 1, CSMF bit 3, CTOF bit 4.
const FCR_CLEAR: u32 = (1 << 0) | (1 << 1) | (1 << 3) | (1 << 4);

// QUADSPI_CCR (14.5.6): the field shifts, and the line-count encoding the mode fields share.
const CCR_IMODE_SHIFT: u32 = 8;
const CCR_ADMODE_SHIFT: u32 = 10;
const CCR_ADSIZE_SHIFT: u32 = 12;
const CCR_DMODE_SHIFT: u32 = 24;
const CCR_FMODE_SHIFT: u32 = 26;
const LINES_ONE: u32 = 0b01;
const LINES_FOUR: u32 = 0b11;
const FMODE_INDIRECT_WRITE: u32 = 0b00;
const FMODE_INDIRECT_READ: u32 = 0b01;
const FMODE_MEMORY_MAPPED: u32 = 0b11;
const ADSIZE_24: u32 = 0b10;
const ADSIZE_32: u32 = 0b11;

/// The controller's interface clock divisor while this module drives it: the AHB clock divided by
/// two (PRESCALER 1, RM0410 14.5.1). After the reset attach a route makes, the AHB clock is the
/// 16 MHz internal oscillator, so the flash sees 8 MHz -- far below the slowest plain-READ ceiling
/// of every part in [`NOR_PARTS`].
const PRESCALER: u32 = 1;

/// Chip select held high between commands for at least two interface clocks (CSHT 1, RM0410
/// 14.5.2), at least every listed part's deselect time at 8 MHz.
const CSHT: u32 = 1;

// ---- the clocks and pins the controller needs ---------------------------------------------------

/// `RCC_AHB1ENR`: RCC at `0x4002 3800` (RM0385 and RM0410 2.2.2) plus offset `0x30` (5.3.10); the
/// GPIO port clock enables are its bits 0 (port A) to 10 (port K).
const RCC_AHB1ENR: u32 = 0x4002_3830;
/// `RCC_AHB3ENR`, offset `0x38` (5.3.12): `QSPIEN` is bit 1.
const RCC_AHB3ENR: u32 = 0x4002_3838;
const RCC_AHB3ENR_QSPIEN: u32 = 1 << 1;

/// GPIO port A's registers; each later port follows at `0x400` (RM0385 and RM0410 2.2.2).
const GPIO_BASE: u32 = 0x4002_0000;
const GPIO_STRIDE: u32 = 0x400;
// Register offsets, RM0385 and RM0410 6.4.
const GPIO_MODER: u32 = 0x00;
const GPIO_OSPEEDR: u32 = 0x08;
const GPIO_PUPDR: u32 = 0x0C;
const GPIO_AFRL: u32 = 0x20;
const GPIO_AFRH: u32 = 0x24;
/// `MODER` 10: alternate function (6.4.1).
const MODER_ALTERNATE: u32 = 0b10;
/// `OSPEEDR` 10: high speed (6.4.3).
const OSPEED_HIGH: u32 = 0b10;

/// `FLASH_OPTCR`, the user option bytes as loaded at reset (RM0385 and RM0410 3.7.6). Its bit 5,
/// `IWDG_SW`, reads 0 when the independent watchdog runs from reset, and bit 4, `WWDG_SW`, reads 0
/// when the window watchdog does.
const FLASH_OPTCR: u32 = 0x4002_3C14;
const OPTCR_IWDG_SW: u32 = 1 << 5;
const OPTCR_WWDG_SW: u32 = 1 << 4;

/// One signal of the quad-SPI bus: which GPIO pin carries it, and the alternate function that
/// connects that pin to the controller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QspiPin {
    /// The port, `0` for port A through `10` for port K.
    pub port: u8,
    /// The pin within the port, `0` to `15`.
    pub pin: u8,
    /// The alternate function number, from the part datasheet's alternate-function table.
    pub function: u8,
}

/// The six pins a board wires to the controller's first bank, in the order clock, chip select,
/// IO0, IO1, IO2, IO3.
///
/// **A BOARD FACT, AND THE FUNCTION NUMBER IS PER PIN.** The same signal sits at a different
/// function number on different pins, and on these parts the chip select's pin takes the
/// controller at one number and its neighbors at another.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QspiWiring {
    /// Clock, chip select, IO0, IO1, IO2 and IO3.
    pub pins: [QspiPin; 6],
}

// ---- the flash parts ---------------------------------------------------------------------------

/// The JEDEC identity command. Every part in [`NOR_PARTS`] answers it with three bytes on one line:
/// N25Q128A datasheet Table 16 ("READ ID 9E/9Fh"), MT25QL512ABB Table 21 and MX25L51245G Table 6
/// ("RDID 9Fh").
pub const READ_IDENTITY: u8 = 0x9F;

/// What this module needs to know about a quad-SPI NOR flash part, read from its datasheet.
///
/// **NO DUMMY COUNT AND NO FAST READ.** The window is read with [`NorPart::read`], the plain read,
/// whose frame is the same on every listed part: an instruction, an address and data, all on one
/// line. A part's fast read frames belong to the program that runs from it, which sets them itself.
#[derive(Debug, PartialEq, Eq)]
pub struct NorPart {
    /// The part number, as the datasheet titles it.
    pub name: &'static str,
    /// What [`READ_IDENTITY`] answers: manufacturer, memory type, capacity.
    pub identity: [u8; 3],
    /// The capacity in bytes.
    pub size: u32,
    /// How many address bytes the commands below take: 3, or 4 for the 4-byte-address commands a
    /// part over 16 MB needs.
    pub address_bytes: u8,
    /// The plain read: no dummy cycles and no alternate bytes.
    pub read: u8,
    /// Page program, of up to one [`page`](Self::page).
    pub page_program: u8,
    /// The page a program may not cross, in bytes.
    pub page: u32,
    /// The erase of one [`block`](Self::block).
    pub block_erase: u8,
    /// The erase granule of [`block_erase`](Self::block_erase), in bytes.
    pub block: u32,
    /// Write enable, which sets the write enable latch.
    pub write_enable: u8,
    /// Read the status register.
    pub read_status: u8,
    /// Reset enable, then reset memory: the part's software reset.
    pub reset: [u8; 2],
    /// The status register's write-in-progress bit.
    pub write_in_progress: u8,
    /// The status register's write enable latch bit.
    pub write_enable_latch: u8,
    /// The status register's block protection bits. While any is set, part of the array refuses
    /// program and erase.
    pub block_protect: u8,
    /// The longest page program the datasheet allows, in microseconds.
    pub page_program_max_us: u32,
    /// The longest block erase the datasheet allows, in milliseconds.
    pub block_erase_max_ms: u32,
    /// The document every value above is read from.
    pub datasheet: &'static str,
}

/// The Micron N25Q128A (128 Mbit, 3 V), the part the 32F746GDISCOVERY carries.
///
/// Micron n25q_128mb_3v_65nm.pdf Rev. T: the ID table (`20h`, `BAh`, `18h`), Table 9 (status
/// register), Table 16 (READ `03h`, PAGE PROGRAM `02h`, SECTOR ERASE `D8h`, WRITE ENABLE `06h`, READ
/// STATUS REGISTER `05h`, RESET ENABLE `66h`, RESET MEMORY `99h`) and Table 38 (page program 5 ms,
/// sector erase 3 s, maximum).
pub const N25Q128A: NorPart = NorPart {
    name: "N25Q128A",
    identity: [0x20, 0xBA, 0x18],
    size: 16 * 1024 * 1024,
    address_bytes: 3,
    read: 0x03,
    page_program: 0x02,
    page: 256,
    block_erase: 0xD8,
    block: 64 * 1024,
    write_enable: 0x06,
    read_status: 0x05,
    reset: [0x66, 0x99],
    write_in_progress: 1 << 0,
    write_enable_latch: 1 << 1,
    block_protect: (1 << 6) | (1 << 4) | (1 << 3) | (1 << 2),
    page_program_max_us: 5_000,
    block_erase_max_ms: 3_000,
    datasheet: "Micron n25q_128mb_3v_65nm.pdf Rev. T",
};

/// The Micron MT25QL512ABB (512 Mbit, 3 V), one of the two parts the 32F769IDISCOVERY's schematic
/// allows.
///
/// Micron MT25Q_QLKT_L_512_ABB_0.pdf Rev. F: Table 19 (`20h`, `BAh`, `20h`), Table 3 (status
/// register), Table 21 (4-BYTE READ `13h`, 4-BYTE PAGE PROGRAM `12h`, 4-BYTE SECTOR ERASE `DCh`,
/// WRITE ENABLE `06h`, READ STATUS REGISTER `05h`, RESET ENABLE `66h`, RESET MEMORY `99h`) and Table
/// 51 (page program 1800 us, sector erase 1 s, maximum).
pub const MT25QL512ABB: NorPart = NorPart {
    name: "MT25QL512ABB",
    identity: [0x20, 0xBA, 0x20],
    size: 64 * 1024 * 1024,
    address_bytes: 4,
    read: 0x13,
    page_program: 0x12,
    page: 256,
    block_erase: 0xDC,
    block: 64 * 1024,
    write_enable: 0x06,
    read_status: 0x05,
    reset: [0x66, 0x99],
    write_in_progress: 1 << 0,
    write_enable_latch: 1 << 1,
    block_protect: (1 << 6) | (1 << 4) | (1 << 3) | (1 << 2),
    page_program_max_us: 1_800,
    block_erase_max_ms: 1_000,
    datasheet: "Micron MT25Q_QLKT_L_512_ABB_0.pdf Rev. F",
};

/// The Macronix MX25L51245G (512 Mbit, 3 V), the other part the 32F769IDISCOVERY's schematic
/// allows.
///
/// Macronix PM2006 Rev. 1.8: Table 6 (`C2h`, `20h`, `1Ah`), Table 7 (status register), Table 5
/// (READ4B `13h`, PP4B `12h`, BE4B `DCh`, WREN `06h`, RDSR `05h`, RSTEN `66h`, RST `99h`) and
/// section 14 (page program 0.75 ms, 64 KB block erase 2 s, maximum).
pub const MX25L51245G: NorPart = NorPart {
    name: "MX25L51245G",
    identity: [0xC2, 0x20, 0x1A],
    size: 64 * 1024 * 1024,
    address_bytes: 4,
    read: 0x13,
    page_program: 0x12,
    page: 256,
    block_erase: 0xDC,
    block: 64 * 1024,
    write_enable: 0x06,
    read_status: 0x05,
    reset: [0x66, 0x99],
    write_in_progress: 1 << 0,
    write_enable_latch: 1 << 1,
    block_protect: (1 << 5) | (1 << 4) | (1 << 3) | (1 << 2),
    page_program_max_us: 750,
    block_erase_max_ms: 2_000,
    datasheet: "Macronix PM2006 Rev. 1.8",
};

/// Every part this module can write.
pub const NOR_PARTS: &[&NorPart] = &[&N25Q128A, &MT25QL512ABB, &MX25L51245G];

impl NorPart {
    /// The FSIZE field for this part: the number of address bits it needs, less one (RM0410
    /// 14.5.2: "Number of bytes in flash memory = 2^[FSIZE+1]").
    #[must_use]
    pub fn size_field(&self) -> u32 {
        self.size.trailing_zeros().saturating_sub(1)
    }

    fn address_size(&self) -> u32 {
        if self.address_bytes == 4 { ADSIZE_32 } else { ADSIZE_24 }
    }

    /// The CCR word for write enable: an instruction and nothing else.
    fn ccr_write_enable(&self) -> u32 {
        command(self.write_enable, LINES_ONE, None, 0, FMODE_INDIRECT_WRITE)
    }

    /// The CCR word for one status read: an instruction, then one line of data in.
    fn ccr_read_status(&self) -> u32 {
        command(self.read_status, LINES_ONE, None, LINES_ONE, FMODE_INDIRECT_READ)
    }

    /// The CCR word for one block erase: an instruction and an address.
    fn ccr_block_erase(&self) -> u32 {
        command(self.block_erase, LINES_ONE, Some(self.address_size()), 0, FMODE_INDIRECT_WRITE)
    }

    /// The CCR word for one page program: an instruction, an address, then data out.
    fn ccr_page_program(&self) -> u32 {
        command(
            self.page_program,
            LINES_ONE,
            Some(self.address_size()),
            LINES_ONE,
            FMODE_INDIRECT_WRITE,
        )
    }

    /// The CCR word that maps the part at [`STM32F7_QUADSPI_WINDOW`] with its plain read.
    fn ccr_mapped_read(&self) -> u32 {
        command(self.read, LINES_ONE, Some(self.address_size()), LINES_ONE, FMODE_MEMORY_MAPPED)
    }
}

/// A QUADSPI_CCR word: the instruction on `instruction_lines`, an address of `address_size` on one
/// line when there is one, data on `data_lines` (0 for none), no alternate bytes and no dummy
/// cycles, in functional mode `fmode`.
fn command(
    instruction: u8,
    instruction_lines: u32,
    address_size: Option<u32>,
    data_lines: u32,
    fmode: u32,
) -> u32 {
    let mut word = u32::from(instruction) | (instruction_lines << CCR_IMODE_SHIFT);
    if let Some(size) = address_size {
        word |= (LINES_ONE << CCR_ADMODE_SHIFT) | (size << CCR_ADSIZE_SHIFT);
    }
    word | (data_lines << CCR_DMODE_SHIFT) | (fmode << CCR_FMODE_SHIFT)
}

// ---- the host side -----------------------------------------------------------------------------

/// How many times a host-side wait reads the status register before it calls the controller
/// stuck. Each read is a probe round trip, so this is a bound in round trips, and one command of
/// this module's finishes within the first few.
const CONTROLLER_POLLS: u32 = 1_000;

/// Refuse a part whose option bytes start a watchdog at reset.
///
/// **THE LOADER RUNS THE CORE, AND A HALTED CORE IS WHAT FREEZES A WATCHDOG.** A write that only
/// halts the core can stop the watchdogs through the debug freeze bits; a loader that runs for
/// seconds cannot, and a hardware watchdog started at reset would reset the part partway through an
/// erase.
///
/// # Errors
/// A watchdog the option bytes start at reset, or a read the probe refuses.
pub fn refuse_hardware_watchdogs<T: TargetAccess + ?Sized>(target: &mut T) -> Result<(), ProbeError> {
    let optcr = target.read_word(FLASH_OPTCR)?;
    if optcr & OPTCR_IWDG_SW == 0 {
        return Err(ProbeError::Device(
            "this part's option bytes start its independent watchdog at reset (IWDG_SW is 0), and \
             the external flash is written by code running on the core, which a watchdog would reset",
        ));
    }
    if optcr & OPTCR_WWDG_SW == 0 {
        return Err(ProbeError::Device(
            "this part's option bytes start its window watchdog at reset (WWDG_SW is 0), and the \
             external flash is written by code running on the core, which a watchdog would reset",
        ));
    }
    Ok(())
}

/// Clock the controller and its pins, connect the pins, and enable the controller with the sample
/// shift set, for a part of `2^(size_field + 1)` bytes.
///
/// Every register is written with the controller idle, as RM0410 14.5.1 and 14.5.2 require, and the
/// pin's function is selected before its mode, so no pin is ever connected to function 0 on the
/// way. The pins are left without pulls: these boards fit their own.
///
/// # Errors
/// A controller that stays busy, or an access the probe refuses.
pub fn bring_up<T: TargetAccess + ?Sized>(
    target: &mut T,
    wiring: &QspiWiring,
    size_field: u32,
) -> Result<(), ProbeError> {
    let mut ports = 0u32;
    for pin in &wiring.pins {
        ports |= 1 << pin.port;
    }
    let ahb1 = target.read_word(RCC_AHB1ENR)?;
    target.write_word(RCC_AHB1ENR, ahb1 | ports)?;
    let ahb3 = target.read_word(RCC_AHB3ENR)?;
    target.write_word(RCC_AHB3ENR, ahb3 | RCC_AHB3ENR_QSPIEN)?;
    for pin in &wiring.pins {
        connect_pin(target, *pin)?;
    }
    stop(target)?;
    configure(target, size_field)
}

/// End whatever the controller is doing and wait for it to go idle, leaving its configuration as
/// it was.
///
/// **THE ABORT IS SET ON TOP OF THE REGISTER AS FOUND**, because writing the abort bit alone would
/// also write zero to the enable bit, and a disabled controller carries out no command after it.
/// A controller in memory-mapped mode stays busy until it is aborted, and every configuration field
/// is writable only while it is idle (RM0410 14.5.1).
fn abort<T: TargetAccess + ?Sized>(target: &mut T) -> Result<(), ProbeError> {
    let cr = target.read_word(STM32F7_QUADSPI + QSPI_CR)?;
    target.write_word(STM32F7_QUADSPI + QSPI_CR, cr | CR_ABORT)?;
    wait_idle(target)
}

/// Abort, then disable the controller so it can be configured again.
fn stop<T: TargetAccess + ?Sized>(target: &mut T) -> Result<(), ProbeError> {
    abort(target)?;
    target.write_word(STM32F7_QUADSPI + QSPI_CR, 0)
}

/// Write the controller's two configuration registers for a part of `2^(size_field + 1)` bytes and
/// enable it, with the controller idle.
fn configure<T: TargetAccess + ?Sized>(target: &mut T, size_field: u32) -> Result<(), ProbeError> {
    wait_idle(target)?;
    let cr = (PRESCALER << CR_PRESCALER_SHIFT) | CR_SSHIFT;
    target.write_word(STM32F7_QUADSPI + QSPI_CR, cr)?;
    let dcr = (size_field << DCR_FSIZE_SHIFT) | (CSHT << DCR_CSHT_SHIFT);
    target.write_word(STM32F7_QUADSPI + QSPI_DCR, dcr)?;
    target.write_word(STM32F7_QUADSPI + QSPI_CR, cr | CR_EN)
}

/// Point `pin` at its alternate function: the function number first, then the speed, then the
/// mode, each a read-modify-write of only that pin's field.
fn connect_pin<T: TargetAccess + ?Sized>(target: &mut T, pin: QspiPin) -> Result<(), ProbeError> {
    let port = GPIO_BASE + GPIO_STRIDE * u32::from(pin.port);
    let number = u32::from(pin.pin);
    let (afr, nibble) =
        if number < 8 { (port + GPIO_AFRL, number) } else { (port + GPIO_AFRH, number - 8) };
    update(target, afr, 0xF << (nibble * 4), u32::from(pin.function) << (nibble * 4))?;
    let shift = number * 2;
    update(target, port + GPIO_OSPEEDR, 0b11 << shift, OSPEED_HIGH << shift)?;
    update(target, port + GPIO_PUPDR, 0b11 << shift, 0)?;
    update(target, port + GPIO_MODER, 0b11 << shift, MODER_ALTERNATE << shift)
}

/// Replace the bits of `mask` at `address` with `value`'s.
fn update<T: TargetAccess + ?Sized>(
    target: &mut T,
    address: u32,
    mask: u32,
    value: u32,
) -> Result<(), ProbeError> {
    let word = target.read_word(address)?;
    target.write_word(address, (word & !mask) | (value & mask))
}

/// Wait for the controller's BUSY flag to clear.
fn wait_idle<T: TargetAccess + ?Sized>(target: &mut T) -> Result<(), ProbeError> {
    for _ in 0..CONTROLLER_POLLS {
        if target.read_word(STM32F7_QUADSPI + QSPI_SR)? & SR_BUSY == 0 {
            return Ok(());
        }
    }
    Err(ProbeError::Timeout("the QUADSPI controller stayed busy"))
}

/// Wait for the transfer-complete flag and clear it; a transfer error is reported instead.
fn wait_complete<T: TargetAccess + ?Sized>(target: &mut T) -> Result<(), ProbeError> {
    for _ in 0..CONTROLLER_POLLS {
        let sr = target.read_word(STM32F7_QUADSPI + QSPI_SR)?;
        if sr & SR_TEF != 0 {
            target.write_word(STM32F7_QUADSPI + QSPI_FCR, FCR_CLEAR)?;
            return Err(ProbeError::Device("the QUADSPI controller flagged a transfer error"));
        }
        if sr & SR_TCF != 0 {
            target.write_word(STM32F7_QUADSPI + QSPI_FCR, FCR_CLEAR)?;
            return Ok(());
        }
    }
    Err(ProbeError::Timeout("the QUADSPI controller never completed a command"))
}

/// Send one instruction with no address and no data, on `lines` lines.
fn instruction<T: TargetAccess + ?Sized>(
    target: &mut T,
    code: u8,
    lines: u32,
) -> Result<(), ProbeError> {
    wait_idle(target)?;
    target.write_word(STM32F7_QUADSPI + QSPI_FCR, FCR_CLEAR)?;
    target.write_word(STM32F7_QUADSPI + QSPI_CCR, command(code, lines, None, 0, FMODE_INDIRECT_WRITE))?;
    wait_complete(target)
}

/// Read `count` bytes answered to `code`, an instruction with no address, on one line.
fn read_register<T: TargetAccess + ?Sized>(
    target: &mut T,
    code: u8,
    count: usize,
) -> Result<Vec<u8>, ProbeError> {
    wait_idle(target)?;
    target.write_word(STM32F7_QUADSPI + QSPI_FCR, FCR_CLEAR)?;
    let length = u32::try_from(count.max(1) - 1).unwrap_or(0);
    target.write_word(STM32F7_QUADSPI + QSPI_DLR, length)?;
    target.write_word(
        STM32F7_QUADSPI + QSPI_CCR,
        command(code, LINES_ONE, None, LINES_ONE, FMODE_INDIRECT_READ),
    )?;
    wait_complete(target)?;
    let mut bytes = Vec::with_capacity(count);
    while bytes.len() < count {
        let word = target.read_word(STM32F7_QUADSPI + QSPI_DR)?;
        bytes.extend_from_slice(&word.to_le_bytes());
    }
    bytes.truncate(count);
    Ok(bytes)
}

/// Reset the part to its power-up state: reset enable then reset memory, sent first on four lines
/// and then on one.
///
/// **BOTH FORMS, SO A PART LEFT IN A FOUR-LINE PROTOCOL HEARS ONE OF THEM.** Each listed part takes
/// its reset in its single-line and its quad protocols (N25Q128A Table 16, MT25QL512ABB Table 21,
/// MX25L51245G Table 5). A part in single-line mode sees the four-line form as two clocks and a
/// chip select that rises off a byte boundary, which every listed part ignores.
///
/// # Errors
/// A controller that stays busy or flags an error.
pub fn reset_part<T: TargetAccess + ?Sized>(target: &mut T, reset: [u8; 2]) -> Result<(), ProbeError> {
    for lines in [LINES_FOUR, LINES_ONE] {
        for code in reset {
            instruction(target, code, lines)?;
        }
    }
    Ok(())
}

/// The three bytes [`READ_IDENTITY`] answers.
///
/// # Errors
/// A controller that stays busy or flags an error.
pub fn read_identity<T: TargetAccess + ?Sized>(target: &mut T) -> Result<[u8; 3], ProbeError> {
    let bytes = read_register(target, READ_IDENTITY, 3)?;
    Ok([bytes[0], bytes[1], bytes[2]])
}

/// The part among `allowed` whose identity is `identity`.
#[must_use]
pub fn part_for(identity: [u8; 3], allowed: &[&'static NorPart]) -> Option<&'static NorPart> {
    allowed.iter().copied().find(|part| part.identity == identity)
}

/// The part's status register.
///
/// # Errors
/// A controller that stays busy or flags an error.
pub fn read_status<T: TargetAccess + ?Sized>(target: &mut T, part: &NorPart) -> Result<u8, ProbeError> {
    Ok(read_register(target, part.read_status, 1)?[0])
}

/// Set the controller's size field for `part`, so an address past the part's end is a transfer
/// error rather than a wrap to its start.
///
/// # Errors
/// A controller that stays busy.
pub fn size_for<T: TargetAccess + ?Sized>(target: &mut T, part: &NorPart) -> Result<(), ProbeError> {
    stop(target)?;
    configure(target, part.size_field())
}

/// Reset the part and map it at [`STM32F7_QUADSPI_WINDOW`] with its plain read, so the whole part
/// reads as memory from there.
///
/// # Errors
/// A controller that stays busy or flags an error.
pub fn map_window<T: TargetAccess + ?Sized>(target: &mut T, part: &NorPart) -> Result<(), ProbeError> {
    abort(target)?;
    reset_part(target, part.reset)?;
    wait_idle(target)?;
    target.write_word(STM32F7_QUADSPI + QSPI_FCR, FCR_CLEAR)?;
    target.write_word(STM32F7_QUADSPI + QSPI_CCR, part.ccr_mapped_read())
}

// ---- the loader -------------------------------------------------------------------------------

/// The loader, assembled from `qspi-loader.s` in this directory and kept beside it so the machine
/// code is reproducible rather than hand-encoded. It is position independent: its one literal load
/// is relative to the program counter.
const LOADER: [u32; 111] = [
    0x4ff0e92d, 0x68254604, 0x68e768a6, 0x8010f8d4, 0x28016860, 0x2802d003, 0x2005d01c,
    0x2f00e060, 0xf000d05d, 0x2800f89f, 0xf000d15a, 0x2800f85c, 0x201bd156, 0x69e060e8,
    0x61ae6168, 0xf86af000, 0xd14d2800, 0xf8a5f000, 0xd1492800, 0x44066aa0, 0xe7e41e7f,
    0xd0422f00, 0x1e416aa0, 0x0101ea06, 0x0901ebb0, 0xbf8845b9, 0xf00046b9, 0x2800f87b,
    0xf000d136, 0x2800f838, 0x201bd132, 0xf1b960e8, 0x61280001, 0x616869e0, 0x46ca61ae,
    0x0f04f1ba, 0x211cd30a, 0xf832f000, 0xd1212800, 0x0b04f858, 0xf1ba6228, 0xe7f10a04,
    0x0f00f1ba, 0x211fd00b, 0xf824f000, 0xd1132800, 0x0b01f818, 0x0020f885, 0x0a01f1ba,
    0xf000e7f0, 0x2800f825, 0xf000d108, 0x2800f860, 0x444ed104, 0x0709ebb7, 0x2000e7ba,
    0xe8bd6326, 0x4a348ff0, 0xf01068a8, 0xd0030f20, 0xd1f91e52, 0x47702004, 0x47702000,
    0x68a84a2e, 0x2005f3c0, 0xd9034288, 0xd1f81e52, 0x47702004, 0x47702000, 0x68a84a28,
    0x0f01f010, 0xf010d10a, 0xd1030f02, 0xd1f61e52, 0x47702004, 0x60e82002, 0x47702000,
    0x60e8201b, 0x47702002, 0xf7ffb500, 0x2800ffd2, 0x201bd10d, 0x200060e8, 0x69a06128,
    0xf7ff6168, 0x2800ffdf, 0xf895d103, 0x63633020, 0xbd002000, 0xf7ffb500, 0x2800ffbe,
    0x201bd110, 0x696060e8, 0xf7ff6168, 0x2800ffcd, 0xf7ffd108, 0x2800ffdd, 0x6a60d104,
    0xbf0c4203, 0x20002003, 0xe92dbd00, 0xf8d44800, 0xf7ffb02c, 0x2800ffcf, 0x6a20d108,
    0xd0044203, 0x0b01f1bb, 0x2001d1f5, 0x2000e000, 0x8800e8bd, 0x00400000,
];

/// Where the loader's pieces live in the chip's SRAM while it runs.
///
/// **IN THE SRAM BOTH F7 LINES SHARE.** SRAM1 starts at `0x2001 0000` on the F74x/F75x (RM0385
/// 2.2.2) and at `0x2002 0000` on the F76x/F77x (RM0410 2.2.2), and runs past `0x2004 BFFF` on
/// both, so the window from `0x2002 0000` below is ordinary SRAM on either part.
///
/// The whole window is overwritten. The core must be halted before the loader is placed, and it is
/// not resumable afterwards without a reset.
const LOADER_TRAP: u32 = 0x2002_0000;
const LOADER_ADDR: u32 = 0x2002_0010;
const LOADER_BLOCK: u32 = 0x2002_0800;
const LOADER_STACK_TOP: u32 = 0x2002_1000;
const LOADER_BUFFER: u32 = 0x2002_1000;

/// The bytes the loader programs per call: one buffer.
pub const LOADER_CHUNK: usize = 64 * 1024;

/// The blocks the loader erases per call.
///
/// **SMALL, SO THE SLOWEST ERASE STILL FITS THE CALL MANY TIMES OVER.** The longest block erase any
/// listed part allows is 3 s, so two blocks are at most 6 s a call, whatever the image's size.
pub const ERASE_BLOCKS_PER_CALL: u32 = 2;

/// How many halt polls one loader call may take before the host gives up on it.
///
/// **COUNTED IN POLLS, WHICH IS TIME ONLY THROUGH THE PROBE'S ROUND TRIP**, so the bound is chosen
/// for a fast probe: at 0.4 ms a poll it is 40 s, more than six times the longest call the
/// datasheets allow ([`ERASE_BLOCKS_PER_CALL`] blocks at the slowest erase), and a slower probe only
/// waits longer.
pub const LOADER_POLLS: u32 = 100_000;

/// How many status reads the loader makes for one erase or page program before it reports a
/// timeout. Each read is at least the 16 bits of a command and its answer at the 8 MHz interface
/// clock, so this is at least 8 s, more than twice the slowest block erase of any listed part, and
/// the loader's own answer arrives before the host's poll budget runs out.
const LOADER_STATUS_READS: u32 = 4_000_000;

// The operation block's word offsets, as `qspi-loader.s` reads them.
const BLOCK_WORDS: usize = 14;
const B_OP: usize = 1;
const B_ADDRESS: usize = 2;
const B_COUNT: usize = 3;
const B_AT: usize = 12;
const B_STATUS: usize = 13;
const OP_ERASE: u32 = 1;
const OP_PROGRAM: u32 = 2;

/// Why an erase or a program through the loader stopped.
#[derive(Debug)]
pub enum QspiError {
    /// The probe could not reach the target, or the call never returned.
    Probe(ProbeError),
    /// The loader returned a failure.
    Loader {
        /// What the loader's code means.
        what: &'static str,
        /// The device address it stopped at.
        at: u32,
        /// The last status byte it read from the part.
        status: u8,
    },
}

impl From<ProbeError> for QspiError {
    fn from(error: ProbeError) -> Self {
        QspiError::Probe(error)
    }
}

impl core::fmt::Display for QspiError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            QspiError::Probe(error) => write!(f, "{error}"),
            QspiError::Loader { what, at, status } => write!(
                f,
                "{what}, at device address {at:#010x} (last status {status:#04x})"
            ),
        }
    }
}

/// What a loader call's non-zero answer means, in the order `qspi-loader.s` numbers them.
fn loader_failure(code: u32) -> &'static str {
    match code {
        1 => "the flash never cleared its write-in-progress bit",
        2 => "the QUADSPI controller flagged an address outside the flash",
        3 => "write enable did not set the flash's write enable latch",
        4 => "the QUADSPI controller never finished a command",
        5 => "the loader was handed an operation it does not know",
        _ => "the loader returned a code it does not define",
    }
}

/// Place the loader in SRAM. The core must already be halted.
///
/// # Errors
/// A write the probe refuses.
pub fn stage_loader<T: TargetAccess + ?Sized>(target: &mut T) -> Result<(), ProbeError> {
    target.write_words(LOADER_ADDR, &LOADER)
}

/// Run one loader operation and report where it stopped when it fails.
fn run<T: TargetAccess + ?Sized>(
    target: &mut T,
    part: &NorPart,
    op: u32,
    address: u32,
    count: u32,
    ccr_op: u32,
    unit: u32,
) -> Result<(), QspiError> {
    let mut block = [0u32; BLOCK_WORDS];
    block[0] = STM32F7_QUADSPI;
    block[B_OP] = op;
    block[B_ADDRESS] = address;
    block[B_COUNT] = count;
    block[4] = LOADER_BUFFER;
    block[5] = part.ccr_write_enable();
    block[6] = part.ccr_read_status();
    block[7] = ccr_op;
    block[8] = u32::from(part.write_in_progress);
    block[9] = u32::from(part.write_enable_latch);
    block[10] = unit;
    block[11] = LOADER_STATUS_READS;
    abort(target)?;
    target.write_words(LOADER_BLOCK, &block)?;
    let frame = CallFrame { sp: LOADER_STACK_TOP, trap: LOADER_TRAP, poll_tries: LOADER_POLLS };
    let code = target.call_target(LOADER_ADDR | 1, &[LOADER_BLOCK], &frame)?;
    if code == 0 {
        return Ok(());
    }
    let at = target.read_word(LOADER_BLOCK + (B_AT as u32) * 4)?;
    let status = target.read_word(LOADER_BLOCK + (B_STATUS as u32) * 4)?;
    Err(QspiError::Loader { what: loader_failure(code), at, status: (status & 0xFF) as u8 })
}

/// Erase `count` blocks of `part` from device address `address`, which must start a block.
///
/// # Errors
/// An address that does not start a block, or the loader's failure.
pub fn erase_blocks<T: TargetAccess + ?Sized>(
    target: &mut T,
    part: &NorPart,
    address: u32,
    count: u32,
) -> Result<(), QspiError> {
    if address % part.block != 0 {
        return Err(ProbeError::Device("an erase must start on a block boundary").into());
    }
    run(target, part, OP_ERASE, address, count, part.ccr_block_erase(), part.block)
}

/// Program `data`, at most [`LOADER_CHUNK`] bytes, into `part` from device address `address`. The
/// bytes must already be erased.
///
/// # Errors
/// A chunk larger than the buffer, or the loader's failure.
pub fn program<T: TargetAccess + ?Sized>(
    target: &mut T,
    part: &NorPart,
    address: u32,
    data: &[u8],
) -> Result<(), QspiError> {
    if data.len() > LOADER_CHUNK {
        return Err(ProbeError::Device("a program call takes at most one loader buffer").into());
    }
    let words: Vec<u32> = data
        .chunks(4)
        .map(|chunk| {
            let mut word = [0xFF; 4];
            word[..chunk.len()].copy_from_slice(chunk);
            u32::from_le_bytes(word)
        })
        .collect();
    target.write_words(LOADER_BUFFER, &words)?;
    let count = u32::try_from(data.len()).unwrap_or(u32::MAX);
    run(target, part, OP_PROGRAM, address, count, part.ccr_page_program(), part.page)
}

#[cfg(test)]
mod tests {
    //! The host side of the route against a modelled controller and part. The loader itself is
    //! machine code and runs only on the chip; what these pin is everything the host composes for
    //! it -- the operation block, the command words -- and every rule the host keeps on its own.

    use super::*;
    use std::collections::{HashMap, VecDeque};

    /// The controller and one part behind it, modelled at the level the device rules live at.
    struct Chip {
        words: HashMap<u32, u32>,
        cr: u32,
        ccr: u32,
        tcf: bool,
        mapped: bool,
        fifo: VecDeque<u8>,
        identity: [u8; 3],
        flash: Vec<u8>,
        status: u8,
        latch: bool,
        reset_enabled: bool,
        resets: u32,
        four_line_instructions: u32,
        /// A part whose answer arrives late enough that the controller loses a bit without the
        /// sample shift.
        late: bool,
        /// Configuration writes made while the controller was busy -- each one a violation of
        /// RM0410 14.5.1 and 14.5.2.
        busy_writes: u32,
        loader_calls: Vec<[u32; BLOCK_WORDS]>,
        log: Vec<String>,
    }

    impl Chip {
        fn new(part: &NorPart) -> Self {
            Chip {
                words: HashMap::new(),
                cr: 0,
                ccr: 0,
                tcf: false,
                mapped: false,
                fifo: VecDeque::new(),
                identity: part.identity,
                flash: vec![0xA5; 4 * 64 * 1024],
                status: 0,
                latch: false,
                reset_enabled: false,
                resets: 0,
                four_line_instructions: 0,
                late: false,
                busy_writes: 0,
                loader_calls: Vec::new(),
                log: Vec::new(),
            }
        }

        fn enabled(&self) -> bool {
            self.cr & CR_EN != 0
        }

        fn execute(&mut self) {
            let instruction = (self.ccr & 0xFF) as u8;
            let lines = (self.ccr >> CCR_IMODE_SHIFT) & 0b11;
            self.tcf = true;
            if lines == LINES_FOUR {
                self.four_line_instructions += 1;
                return;
            }
            let previous = std::mem::replace(&mut self.reset_enabled, false);
            match instruction {
                0x66 => self.reset_enabled = true,
                0x99 if previous => {
                    self.resets += 1;
                    self.latch = false;
                }
                0x9F => {
                    let mut id = self.identity;
                    if self.late && self.cr & CR_SSHIFT == 0 {
                        id[0] &= !0x40;
                    }
                    self.fifo.extend(id);
                }
                0x05 => self.fifo.push_back(self.status | (u8::from(self.latch) << 1)),
                0x06 => self.latch = true,
                other => self.log.push(format!("unmodelled instruction {other:#04x}")),
            }
        }
    }

    const SRAM: std::ops::Range<u32> = 0x2002_0000..0x2003_1000;

    impl TargetAccess for Chip {
        fn connect(&mut self) -> Result<(), ProbeError> {
            Ok(())
        }
        fn read_idcode(&mut self) -> Result<u32, ProbeError> {
            Ok(0x5ba0_2477)
        }
        fn init_mem(&mut self) -> Result<(), ProbeError> {
            Ok(())
        }
        fn read_word(&mut self, address: u32) -> Result<u32, ProbeError> {
            match address {
                a if a == STM32F7_QUADSPI + QSPI_CR => Ok(self.cr),
                a if a == STM32F7_QUADSPI + QSPI_DR => {
                    let mut word = [0u8; 4];
                    for byte in &mut word {
                        *byte = self.fifo.pop_front().unwrap_or(0);
                    }
                    Ok(u32::from_le_bytes(word))
                }
                a if a == STM32F7_QUADSPI + QSPI_SR => {
                    let level = u32::try_from(self.fifo.len()).unwrap_or(32);
                    Ok((u32::from(self.tcf) << 1) | (u32::from(self.mapped) << 5) | (level << 8))
                }
                a if (STM32F7_QUADSPI_WINDOW..STM32F7_QUADSPI_WINDOW + 0x1000_0000).contains(&a) => {
                    if !self.mapped {
                        return Err(ProbeError::Device("the window was read before it was mapped"));
                    }
                    let dummy = (self.ccr >> 18) & 0x1F;
                    let alternate = (self.ccr >> 14) & 0b11;
                    let shift = if dummy + alternate == 0 { 0 } else { 2 };
                    let offset = (a - STM32F7_QUADSPI_WINDOW) as usize + shift;
                    let cell = self.flash.get(offset..offset + 4).ok_or(ProbeError::Device("past the part"))?;
                    Ok(u32::from_le_bytes([cell[0], cell[1], cell[2], cell[3]]))
                }
                a if a == FLASH_OPTCR => Ok(0xFFFF_AAFD),
                a => Ok(*self.words.get(&a).unwrap_or(&0)),
            }
        }
        fn write_word(&mut self, address: u32, value: u32) -> Result<(), ProbeError> {
            match address {
                a if a == STM32F7_QUADSPI + QSPI_CR => {
                    if value & CR_ABORT != 0 {
                        self.mapped = false;
                        self.fifo.clear();
                        self.tcf = true;
                    } else if self.mapped && value != self.cr {
                        self.busy_writes += 1;
                    }
                    self.cr = value & !CR_ABORT;
                }
                a if a == STM32F7_QUADSPI + QSPI_DCR => {
                    if self.mapped {
                        self.busy_writes += 1;
                    }
                }
                a if a == STM32F7_QUADSPI + QSPI_FCR => {
                    if value & 0b10 != 0 {
                        self.tcf = false;
                    }
                }
                a if a == STM32F7_QUADSPI + QSPI_DLR => {}
                a if a == STM32F7_QUADSPI + QSPI_CCR => {
                    if self.mapped {
                        self.busy_writes += 1;
                    }
                    self.ccr = value;
                    if !self.enabled() {
                        return Ok(());
                    }
                    let fmode = (value >> CCR_FMODE_SHIFT) & 0b11;
                    let admode = (value >> CCR_ADMODE_SHIFT) & 0b11;
                    let dmode = (value >> CCR_DMODE_SHIFT) & 0b11;
                    if fmode == FMODE_MEMORY_MAPPED {
                        self.mapped = true;
                    } else if admode == 0 && (fmode == FMODE_INDIRECT_READ || dmode == 0) {
                        self.execute();
                    }
                }
                a => {
                    self.words.insert(a, value);
                }
            }
            Ok(())
        }
        fn read_words_into(&mut self, address: u32, out: &mut [u32]) -> Result<(), ProbeError> {
            for (index, slot) in out.iter_mut().enumerate() {
                *slot = self.read_word(address + (index * 4) as u32)?;
            }
            Ok(())
        }
        fn write_words(&mut self, address: u32, words: &[u32]) -> Result<(), ProbeError> {
            assert!(SRAM.contains(&address), "a burst outside the loader's SRAM: {address:#010x}");
            for (index, word) in words.iter().enumerate() {
                self.write_word(address + (index * 4) as u32, *word)?;
            }
            Ok(())
        }
        fn read_byte(&mut self, _address: u32) -> Result<u8, ProbeError> {
            unreachable!("the FIFO is read a word at a time: a probe's byte read may be a word read")
        }
        fn write_byte(&mut self, _address: u32, _value: u8) -> Result<(), ProbeError> {
            unreachable!("the host writes no bytes")
        }
        fn read_halfword(&mut self, _address: u32) -> Result<u16, ProbeError> {
            unreachable!("the host reads no halfwords")
        }
        fn write_halfword(&mut self, _address: u32, _value: u16) -> Result<(), ProbeError> {
            unreachable!("the host writes no halfwords")
        }
        fn halt(&mut self) -> Result<(), ProbeError> {
            Ok(())
        }
        fn resume(&mut self) -> Result<(), ProbeError> {
            unreachable!("run control is call_target's")
        }
        fn step(&mut self) -> Result<(), ProbeError> {
            unreachable!("run control is call_target's")
        }
        fn is_halted(&mut self) -> Result<bool, ProbeError> {
            unreachable!("run control is call_target's")
        }
        fn wait_halted(&mut self) -> Result<(), ProbeError> {
            unreachable!("run control is call_target's")
        }
        fn reset_and_run(&mut self) -> Result<(), ProbeError> {
            unreachable!("this module releases nothing")
        }
        fn reset_and_halt(&mut self) -> Result<(), ProbeError> {
            unreachable!("this module resets nothing")
        }
        fn set_reset(&mut self, _assert: bool) -> Result<u8, ProbeError> {
            unreachable!("this module drives no reset line")
        }
        fn read_core_reg(&mut self, _selector: u8) -> Result<u32, ProbeError> {
            unreachable!("core registers are call_target's")
        }
        fn write_core_reg(&mut self, _selector: u8, _value: u32) -> Result<(), ProbeError> {
            unreachable!("core registers are call_target's")
        }
        fn arm_reset_catch(&mut self) -> Result<(), ProbeError> {
            unreachable!("no reset catch")
        }
        fn disarm_reset_catch(&mut self) -> Result<(), ProbeError> {
            unreachable!("no reset catch")
        }
        fn set_breakpoint(&mut self, _address: u32) -> Result<(), ProbeError> {
            unreachable!("no breakpoints")
        }
        fn clear_breakpoint(&mut self) -> Result<(), ProbeError> {
            unreachable!("no breakpoints")
        }
        fn set_breakpoints(&mut self, _addresses: &[u32]) -> Result<(), ProbeError> {
            unreachable!("no breakpoints")
        }
        /// The loader's effect, applied to the modelled part: what `qspi-loader.s` does with the
        /// block the host wrote.
        fn call_target(&mut self, address: u32, args: &[u32], frame: &CallFrame) -> Result<u32, ProbeError> {
            assert_eq!(address, LOADER_ADDR | 1, "the loader is entered in Thumb state at its address");
            assert_eq!(args, [LOADER_BLOCK]);
            assert_eq!((frame.sp, frame.trap), (LOADER_STACK_TOP, LOADER_TRAP));
            assert!(!self.mapped, "the loader's first wait is for an idle controller");
            let mut block = [0u32; BLOCK_WORDS];
            for (index, slot) in block.iter_mut().enumerate() {
                *slot = self.read_word(LOADER_BLOCK + 4 * index as u32)?;
            }
            self.loader_calls.push(block);
            let opcode = (block[7] & 0xFF) as u8;
            let at = block[B_ADDRESS] as usize;
            let count = block[B_COUNT] as usize;
            match block[B_OP] {
                OP_ERASE => {
                    let unit = block[10] as usize;
                    self.log.push(format!("erase {count} x {unit:#x} at {at:#x} with {opcode:#04x}"));
                    self.flash
                        .get_mut(at..at + count * unit)
                        .ok_or(ProbeError::Device("an erase past the part"))?
                        .fill(0xFF);
                }
                OP_PROGRAM => {
                    self.log.push(format!("program {count} at {at:#x} with {opcode:#04x}"));
                    for offset in 0..count {
                        let source = block[4] + offset as u32;
                        let word = self.read_word(source & !3)?;
                        let byte = word.to_le_bytes()[(source & 3) as usize];
                        let cell =
                            self.flash.get_mut(at + offset).ok_or(ProbeError::Device("a program past the part"))?;
                        *cell &= byte;
                    }
                }
                _ => return Ok(5),
            }
            Ok(0)
        }
    }

    const WIRING: QspiWiring = QspiWiring {
        pins: [
            QspiPin { port: 1, pin: 2, function: 9 },
            QspiPin { port: 1, pin: 6, function: 10 },
            QspiPin { port: 2, pin: 9, function: 9 },
            QspiPin { port: 2, pin: 10, function: 9 },
            QspiPin { port: 4, pin: 2, function: 9 },
            QspiPin { port: 3, pin: 13, function: 9 },
        ],
    };

    /// A part whose first identity byte loses a bit without the sample shift. The bring-up sets it
    /// before the read, so the part is named correctly.
    #[test]
    fn the_identity_is_read_with_the_sample_shift_already_set() {
        let mut chip = Chip::new(&MX25L51245G);
        chip.late = true;
        bring_up(&mut chip, &WIRING, 25).expect("up");
        let identity = read_identity(&mut chip).expect("an identity");
        assert_eq!(identity, [0xC2, 0x20, 0x1A], "not 0x82: the shift was set first");
        assert_eq!(part_for(identity, &[&MT25QL512ABB, &MX25L51245G]), Some(&MX25L51245G));
        assert_eq!(part_for(identity, &[&MT25QL512ABB]), None, "a part the board does not allow");
    }

    /// The control for the test above: the same part without the shift answers the wrong byte, so
    /// the model can fail the way the hardware does.
    #[test]
    fn without_the_sample_shift_the_model_loses_the_bit() {
        let mut chip = Chip::new(&MX25L51245G);
        chip.late = true;
        bring_up(&mut chip, &WIRING, 25).expect("up");
        chip.cr &= !CR_SSHIFT;
        assert_eq!(read_identity(&mut chip).expect("an identity")[0], 0x82);
    }

    /// A reset goes out in both protocols, and the single-line one is the one a part in its
    /// power-up protocol acts on.
    #[test]
    fn a_reset_is_sent_on_four_lines_and_then_on_one() {
        let mut chip = Chip::new(&N25Q128A);
        bring_up(&mut chip, &WIRING, 23).expect("up");
        reset_part(&mut chip, N25Q128A.reset).expect("a reset");
        assert_eq!(chip.four_line_instructions, 2, "66h and 99h on four lines");
        assert_eq!(chip.resets, 1, "and one reset the part took, from the single-line pair");
    }

    /// The window is mapped after a reset, with the part's plain read: no dummy phase and no
    /// alternate byte, so it reads true whatever the last program set.
    #[test]
    fn the_window_is_mapped_with_the_plain_read_after_a_reset() {
        for part in NOR_PARTS {
            let mut chip = Chip::new(part);
            chip.flash[..8].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
            bring_up(&mut chip, &WIRING, part.size_field()).expect("up");
            size_for(&mut chip, part).expect("sized");
            let before = chip.resets;
            map_window(&mut chip, part).expect("mapped");
            assert_eq!(chip.resets, before + 1, "{}: a reset before the read", part.name);
            assert_eq!(chip.ccr & 0xFF, u32::from(part.read), "{}", part.name);
            assert_eq!((chip.ccr >> 18) & 0x1F, 0, "{}: no dummy cycles", part.name);
            assert_eq!((chip.ccr >> 14) & 0b11, 0, "{}: no alternate bytes", part.name);
            assert_eq!(chip.read_word(STM32F7_QUADSPI_WINDOW).expect("a read"), 0x0403_0201);
            assert_eq!(chip.busy_writes, 0, "{}: nothing configured while busy", part.name);
        }
    }

    /// A mapped controller is busy until aborted. The abort keeps the controller enabled, so the
    /// reset after it is carried out and a second mapping works.
    #[test]
    fn a_mapped_controller_is_aborted_and_still_enabled() {
        let mut chip = Chip::new(&MX25L51245G);
        bring_up(&mut chip, &WIRING, 25).expect("up");
        map_window(&mut chip, &MX25L51245G).expect("mapped once");
        map_window(&mut chip, &MX25L51245G).expect("mapped again");
        assert_eq!(chip.resets, 2);
        assert!(chip.enabled());
    }

    /// The loader is handed this part's own commands at this part's address size, and what it
    /// writes reads back through the window.
    #[test]
    fn the_loader_gets_each_parts_own_commands() {
        for part in NOR_PARTS {
            let mut chip = Chip::new(part);
            bring_up(&mut chip, &WIRING, part.size_field()).expect("up");
            size_for(&mut chip, part).expect("sized");
            stage_loader(&mut chip).expect("staged");
            erase_blocks(&mut chip, part, 0, 2).expect("erased");
            let data: Vec<u8> = (0..300u32).map(|n| (n % 251) as u8).collect();
            program(&mut chip, part, 0x100, &data).expect("programmed");
            let [erase, write] = [chip.loader_calls[0], chip.loader_calls[1]];
            assert_eq!(erase[7] & 0xFF, u32::from(part.block_erase), "{}", part.name);
            assert_eq!(write[7] & 0xFF, u32::from(part.page_program), "{}", part.name);
            let size = if part.address_bytes == 4 { ADSIZE_32 } else { ADSIZE_24 };
            for word in [erase[7], write[7]] {
                assert_eq!((word >> CCR_ADSIZE_SHIFT) & 0b11, size, "{}", part.name);
                assert_eq!((word >> CCR_IMODE_SHIFT) & 0b11, LINES_ONE, "{}: one line", part.name);
            }
            assert_eq!(write[10], part.page);
            assert_eq!(erase[10], part.block);
            assert_eq!(write[5] & 0xFF, u32::from(part.write_enable));
            assert_eq!(write[6] & 0xFF, u32::from(part.read_status));
            map_window(&mut chip, part).expect("mapped");
            let mut back = vec![0u32; 75];
            chip.read_words_into(STM32F7_QUADSPI_WINDOW + 0x100, &mut back).expect("read");
            let back: Vec<u8> = back.iter().flat_map(|word| word.to_le_bytes()).collect();
            assert_eq!(back, data, "{}", part.name);
        }
    }

    /// A loader call after a read through the window finds the controller idle, because the host
    /// aborts the mapping first.
    #[test]
    fn a_loader_call_after_a_mapping_finds_the_controller_idle() {
        let mut chip = Chip::new(&N25Q128A);
        bring_up(&mut chip, &WIRING, 23).expect("up");
        stage_loader(&mut chip).expect("staged");
        map_window(&mut chip, &N25Q128A).expect("mapped");
        erase_blocks(&mut chip, &N25Q128A, 0x1_0000, 1).expect("the call found an idle controller");
    }

    /// The datasheet facts easiest to copy from a sibling, held against each other.
    #[test]
    fn the_part_table_holds_each_datasheet_apart() {
        assert_eq!(N25Q128A.size_field(), 23);
        assert_eq!(MT25QL512ABB.size_field(), 25);
        assert_eq!(MX25L51245G.size_field(), 25);
        for part in NOR_PARTS {
            assert_eq!(part.block_protect & (part.write_in_progress | part.write_enable_latch), 0, "{}", part.name);
            assert!(part.page.is_power_of_two() && part.block.is_power_of_two(), "{}", part.name);
            assert_eq!(part.reset, [0x66, 0x99], "{}", part.name);
            assert!(part.size <= STM32F7_QUADSPI_WINDOW_LIMIT, "{}", part.name);
            assert_eq!(part.address_bytes == 4, part.size > 16 * 1024 * 1024, "{}", part.name);
        }
        let identities: std::collections::HashSet<[u8; 3]> = NOR_PARTS.iter().map(|part| part.identity).collect();
        assert_eq!(identities.len(), NOR_PARTS.len(), "no two parts answer alike");
        assert_ne!(MX25L51245G.block_protect, MT25QL512ABB.block_protect);
    }

    /// The longest call the datasheets allow fits the loader's own status budget and the host's
    /// poll budget with room to spare.
    #[test]
    fn every_call_fits_its_budgets() {
        let slowest_erase_ms = NOR_PARTS.iter().map(|part| part.block_erase_max_ms).max().unwrap_or(0);
        let loader_floor_ms = u64::from(LOADER_STATUS_READS) * 2 / 1000;
        assert!(loader_floor_ms >= 2 * u64::from(slowest_erase_ms), "{loader_floor_ms} ms per operation");
        let host_ms = u64::from(LOADER_POLLS) * 4 / 10;
        let call_ms = u64::from(ERASE_BLOCKS_PER_CALL) * u64::from(slowest_erase_ms);
        assert!(host_ms >= 5 * call_ms, "{host_ms} ms of polls for a {call_ms} ms call");
        let pages = (LOADER_CHUNK as u64) / 256;
        let slowest_page_us = NOR_PARTS.iter().map(|part| part.page_program_max_us).max().unwrap_or(0);
        assert!(host_ms * 1000 >= 5 * pages * u64::from(slowest_page_us), "a buffer's programming");
    }

    /// The operation block's offsets are the loader's: the words the host writes are the words
    /// `qspi-loader.s` reads.
    #[test]
    fn the_block_layout_is_the_loaders() {
        let source = include_str!("qspi-loader.s");
        let equ = |name: &str| -> u32 {
            let label = format!("{name},");
            let line = source
                .lines()
                .find(|line| line.split_whitespace().nth(1) == Some(label.as_str()))
                .unwrap_or_else(|| panic!("{name} is not in the loader"));
            line.split_whitespace().nth(2).and_then(|value| value.parse().ok()).expect("a decimal value")
        };
        assert_eq!(equ("B_OP"), 4 * B_OP as u32);
        assert_eq!(equ("B_ADDRESS"), 4 * B_ADDRESS as u32);
        assert_eq!(equ("B_COUNT"), 4 * B_COUNT as u32);
        assert_eq!(equ("B_AT"), 4 * B_AT as u32);
        assert_eq!(equ("B_STATUS"), 4 * B_STATUS as u32);
        assert_eq!(equ("OP_ERASE"), OP_ERASE);
        assert_eq!(equ("OP_PROGRAM"), OP_PROGRAM);
        assert_eq!(BLOCK_WORDS as u32 * 4, equ("B_STATUS") + 4);
        assert!(LOADER_ADDR + 4 * LOADER.len() as u32 <= LOADER_BLOCK, "the loader runs into its block");
        assert!(LOADER_BLOCK + 4 * BLOCK_WORDS as u32 <= LOADER_STACK_TOP - 256, "the block runs into the stack");
        assert_eq!(*LOADER.last().expect("words"), 0x0040_0000, "the stuck limit is the last word");
    }
}
