//! The module's serial (SPI) flash -- its own firmware store -- accessed through the same
//! register wire the driver already speaks, per the official host driver's flash layer
//! (`spi_flash.c`) and its download-mode entry (`nmdrv.c`/`nmasic.c`): the WINC's flash
//! controller is a small register block the host drives directly, with data staged through
//! the shared packet memory. Everything here runs with the WINC's CPU HALTED (download
//! mode), so the firmware cannot race its own store -- which is exactly what a firmware
//! UPDATE needs: dump, erase, program, verify, over any transport that implements the
//! [`SpiBus`](crate::SpiBus) wire, whether a debug probe bit-bangs the bus or a host controller
//! drives it in hardware.

use crate::SpiBus;
use crate::spi::{Link, SpiError};

/// The flash-controller register block.
const SPI_FLASH_BASE: u32 = 0x10200;
const SPI_FLASH_CMD_CNT: u32 = SPI_FLASH_BASE + 0x04;
const SPI_FLASH_DATA_CNT: u32 = SPI_FLASH_BASE + 0x08;
const SPI_FLASH_BUF1: u32 = SPI_FLASH_BASE + 0x0c;
const SPI_FLASH_BUF2: u32 = SPI_FLASH_BASE + 0x10;
const SPI_FLASH_BUF_DIR: u32 = SPI_FLASH_BASE + 0x14;
const SPI_FLASH_TR_DONE: u32 = SPI_FLASH_BASE + 0x18;
const SPI_FLASH_DMA_ADDR: u32 = SPI_FLASH_BASE + 0x1c;

/// A scratch module register the byte-answer commands (RDID, RDSR) DMA their reply into.
const DUMMY_REGISTER: u32 = 0x1084;
/// The shared packet memory used to stage flash data blocks.
const HOST_SHARE_MEM: u32 = 0x000d_0000;
/// Global reset + CPU halt registers (nmasic.c): the download-mode entry.
const NMI_GLB_RESET_0: u32 = 0x1400;
const CPU_HALT_REG: u32 = 0x1118;
/// Clockless wake registers (nmasic.c `chip_wake`): asserting the host-wakeup + clock-enable
/// bits here turns the module's main clock ON and KEEPS it on across the global reset, so the
/// clocked CPU-halt accesses below survive it. All three are clockless (offset <= 0x30), so
/// the [`Link`] reaches them even before the main clock runs.
const HOST_CORT_COMM: u32 = 0x0b;
const WAKE_CLK_REG: u32 = 0x01;
const CLOCKS_EN_REG: u32 = 0x0f;
/// The pin-mux register the flash-enable dance reshapes on the 3A0 revision.
const PIN_MUX_FLASH: u32 = 0x1410;
/// ROM interrupt disable (silences the ROM UART in download mode).
const ROM_INTR_DISABLE: u32 = 0x2_0300;

/// The flash's erase granularity (a 4 KB sector, the smallest erasable unit).
pub const SECTOR: u32 = 4 * 1024;
/// The flash's program granularity (a 256-byte page, the largest single program).
pub const PAGE: u32 = 256;
/// The staging chunk size through the shared packet memory. Held at 1000 (< the SPI layer's
/// 1024-byte data-packet unit) so each [`Link::read_block`]/[`Link::write_block`] moves a
/// SINGLE data packet -- the block protocol streams one header per packet, and a larger
/// staged read would make `read_block` mistake mid-stream data for the next packet's header.
const CHUNK: u32 = 1000;

/// A bounded TR_DONE / status poll that gave up.
const POLL_BUDGET: u32 = 200_000;

/// One flash-controller command: up to five command bytes (the low four LE-packed into
/// BUF1, the fifth in BUF2), a data count, the buffer-direction mask, and where the data
/// DMAs. `extra_cnt` ORs into CMD_CNT beyond `count | GO` (the page-program length field).
fn flash_cmd<S: SpiBus>(
    link: &mut Link<S>,
    cmd: &[u8],
    data_cnt: u32,
    buf_dir: u32,
    dma_addr: u32,
    extra_cnt: u32,
) -> Result<(), SpiError> {
    let mut buf1 = 0u32;
    for (index, &byte) in cmd.iter().take(4).enumerate() {
        buf1 |= u32::from(byte) << (8 * index);
    }
    link.write_reg(SPI_FLASH_DATA_CNT, data_cnt)?;
    link.write_reg(SPI_FLASH_BUF1, buf1)?;
    if cmd.len() > 4 {
        link.write_reg(SPI_FLASH_BUF2, u32::from(cmd[4]))?;
    }
    link.write_reg(SPI_FLASH_BUF_DIR, buf_dir)?;
    link.write_reg(SPI_FLASH_DMA_ADDR, dma_addr)?;
    link.write_reg(SPI_FLASH_CMD_CNT, cmd.len() as u32 | (1 << 7) | extra_cnt)?;
    for _ in 0..POLL_BUDGET {
        if link.read_reg(SPI_FLASH_TR_DONE)? == 1 {
            return Ok(());
        }
    }
    Err(SpiError::Timeout)
}

/// The flash's JEDEC id (command 0x9F): manufacturer + type + capacity bytes.
pub fn flash_id<S: SpiBus>(link: &mut Link<S>) -> Result<u32, SpiError> {
    flash_cmd(link, &[0x9f], 4, 0x1, DUMMY_REGISTER, 0)?;
    link.read_reg(DUMMY_REGISTER)
}

/// The flash status register (command 0x05); bit 0 = write/erase in progress.
fn flash_status<S: SpiBus>(link: &mut Link<S>) -> Result<u8, SpiError> {
    flash_cmd(link, &[0x05], 4, 0x1, DUMMY_REGISTER, 0)?;
    Ok((link.read_reg(DUMMY_REGISTER)? & 0xff) as u8)
}

/// Waits out a program/erase in progress (status bit 0).
fn flash_wait_idle<S: SpiBus>(link: &mut Link<S>) -> Result<(), SpiError> {
    for _ in 0..POLL_BUDGET {
        if flash_status(link)? & 0x01 == 0 {
            return Ok(());
        }
    }
    Err(SpiError::Timeout)
}

fn write_enable<S: SpiBus>(link: &mut Link<S>) -> Result<(), SpiError> {
    flash_cmd(link, &[0x06], 0, 0x1, 0, 0)
}

fn write_disable<S: SpiBus>(link: &mut Link<S>) -> Result<(), SpiError> {
    flash_cmd(link, &[0x04], 0, 0x1, 0, 0)
}

/// Puts the module in DOWNLOAD MODE: global reset, the CPU halted before it can boot, ROM
/// interrupts silenced -- the required state for any flash access (SDG 13.x). The caller
/// re-runs [`Link::init`] afterwards (the reset clears the SPI packet-size config, exactly
/// as the reference flow re-inits its bus after the global reset) -- this function does it.
/// `delay_ms` supplies the reset settle (the reference uses 50 ms).
pub fn enter_download_mode<S: SpiBus>(
    link: &mut Link<S>,
    delay_ms: &mut dyn FnMut(u32),
) -> Result<(), SpiError> {
    wake_clock(link, delay_ms)?;
    let halt = link.read_reg(CPU_HALT_REG)?;
    link.write_reg(CPU_HALT_REG, halt | 1)?;
    let reset = link.read_reg(NMI_GLB_RESET_0)?;
    if reset & (1 << 10) != 0 {
        link.write_reg(NMI_GLB_RESET_0, reset & !(1 << 10))?;
        let _ = link.read_reg(NMI_GLB_RESET_0)?;
    }
    let mux = link.read_reg(PIN_MUX_FLASH)?;
    link.write_reg(PIN_MUX_FLASH, (mux & !(0x7777 << 12)) | (0x1111 << 12))?;
    flash_cmd(link, &[0xab], 0, 0x1, 0, 0)?;
    link.write_reg(ROM_INTR_DISABLE, 0)
}

/// Turns the module's main clock on via the clockless wake registers (nmasic.c `chip_wake`):
/// assert the host-comm wake bit and the wake-clock bit, then poll the clock-enable status
/// until the clock reports ready. Idempotent (each bit is only set if clear).
fn wake_clock<S: SpiBus>(
    link: &mut Link<S>,
    delay_ms: &mut dyn FnMut(u32),
) -> Result<(), SpiError> {
    let comm = link.read_reg(HOST_CORT_COMM)?;
    if comm & 1 == 0 {
        link.write_reg(HOST_CORT_COMM, comm | 1)?;
    }
    let clk = link.read_reg(WAKE_CLK_REG)?;
    if clk & 2 == 0 {
        link.write_reg(WAKE_CLK_REG, clk | 2)?;
    }
    for _ in 0..8 {
        if link.read_reg(CLOCKS_EN_REG)? & 4 != 0 {
            return Ok(());
        }
        delay_ms(2);
    }
    Err(SpiError::Timeout)
}

/// Reads `buf.len()` bytes from flash `offset`: each block is pulled into the shared
/// packet memory by the controller's fast-read (0x0B + 24-bit address + dummy), then
/// block-read over the wire.
pub fn flash_read<S: SpiBus>(
    link: &mut Link<S>,
    mut offset: u32,
    buf: &mut [u8],
) -> Result<(), SpiError> {
    for chunk in buf.chunks_mut(CHUNK as usize) {
        let cmd = [
            0x0b,
            (offset >> 16) as u8,
            (offset >> 8) as u8,
            offset as u8,
            0xa5,
        ];
        flash_cmd(link, &cmd, chunk.len() as u32, 0x1f, HOST_SHARE_MEM, 0)?;
        link.read_block(HOST_SHARE_MEM, chunk)?;
        offset += chunk.len() as u32;
    }
    Ok(())
}

/// Erases every 4 KB sector overlapping `[offset, offset + len)`.
pub fn flash_erase<S: SpiBus>(link: &mut Link<S>, offset: u32, len: u32) -> Result<(), SpiError> {
    let first = offset / SECTOR;
    let last = (offset + len.max(1) - 1) / SECTOR;
    for sector in first..=last {
        let addr = sector * SECTOR;
        write_enable(link)?;
        let cmd = [0x20, (addr >> 16) as u8, (addr >> 8) as u8, addr as u8];
        flash_cmd(link, &cmd, 0, 0x0f, 0, 0)?;
        flash_wait_idle(link)?;
    }
    Ok(())
}

/// Programs `data` at flash `offset` (the sectors must already be erased): page-aligned
/// chunks staged through the shared packet memory, each written with page-program (0x02)
/// carrying its length in the command-count register's size field.
pub fn flash_write<S: SpiBus>(
    link: &mut Link<S>,
    mut offset: u32,
    data: &[u8],
) -> Result<(), SpiError> {
    let mut remaining = data;
    while !remaining.is_empty() {
        let page_room = PAGE - (offset % PAGE);
        let take = remaining.len().min(page_room as usize);
        let (chunk, rest) = remaining.split_at(take);
        write_enable(link)?;
        link.write_block(HOST_SHARE_MEM, chunk)?;
        let cmd = [0x02, (offset >> 16) as u8, (offset >> 8) as u8, offset as u8];
        flash_cmd(
            link,
            &cmd,
            0,
            0x0f,
            HOST_SHARE_MEM,
            ((chunk.len() as u32) & 0xfffff) << 8,
        )?;
        flash_wait_idle(link)?;
        offset += take as u32;
        remaining = rest;
    }
    write_disable(link)
}
