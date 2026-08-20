//! The STM32 SDMMC controller, behind the portable [`SdmmcHost`] seam.

#![no_std]
#![forbid(unsafe_code)]

use lamella_mmio::{read32, write32};
use lamella_sd_core::SECTOR_LEN;
use lamella_sd_sdmmc::{BusWidth, Response, ResponseKind, SdmmcError, SdmmcHost};

/// SDMMC1's register block (RM0410 memory map).
pub const SDMMC1_BASE: u32 = 0x4001_2C00;
/// SDMMC2's register block. **This is the one an STM32F769I-DISCO's on-board microSD socket is
/// wired to**, in four-bit mode, on PB3/PB4/PD6/PD7/PG9/PG10.
pub const SDMMC2_BASE: u32 = 0x4001_1C00;

const POWER: u32 = 0x00;
const CLKCR: u32 = 0x04;
const ARG: u32 = 0x08;
const CMD: u32 = 0x0C;
const RESP1: u32 = 0x14;
const DTIMER: u32 = 0x24;
const DLEN: u32 = 0x28;
const DCTRL: u32 = 0x2C;
const STA: u32 = 0x34;
const ICR: u32 = 0x38;
const FIFO: u32 = 0x80;

const PWRCTRL_ON: u32 = 0b11;

const CLKCR_CLKDIV_MASK: u32 = 0xFF;
const CLKCR_CLKEN: u32 = 1 << 8;
const CLKCR_BYPASS: u32 = 1 << 10;
/// CLKCR bit 14. **The controller stops `SDMMC_CK` and freezes its state machines whenever the
/// FIFO cannot keep up**, while leaving the APB interface alive so the CPU can still drain it
/// (RM0410 39.7). Disabled after reset.
///
/// This is what makes a polled driver viable at a real clock: without it a four-bit bus at 24 MHz
/// delivers 12 MB/s into a 32-word FIFO, and the first moment the drain loses the race is an
/// overrun rather than a slow transfer. It is enabled in [`Stm32Sdmmc::power_on`].
const CLKCR_HWFC_EN: u32 = 1 << 14;
const CLKCR_WIDBUS_SHIFT: u32 = 11;
const CLKCR_WIDBUS_MASK: u32 = 0b11 << CLKCR_WIDBUS_SHIFT;
/// WIDBUS 00: one data line. 01: four. (10 is eight, which no SD card uses.)
const WIDBUS_1BIT: u32 = 0b00 << CLKCR_WIDBUS_SHIFT;
const WIDBUS_4BIT: u32 = 0b01 << CLKCR_WIDBUS_SHIFT;

const CMD_WAITRESP_SHIFT: u32 = 6;
const CMD_WAITRESP_NONE: u32 = 0b00 << CMD_WAITRESP_SHIFT;
const CMD_WAITRESP_SHORT: u32 = 0b01 << CMD_WAITRESP_SHIFT;
const CMD_WAITRESP_LONG: u32 = 0b11 << CMD_WAITRESP_SHIFT;
const CMD_CPSMEN: u32 = 1 << 10;

const DCTRL_DTEN: u32 = 1 << 0;
/// DTDIR 1 is card-to-controller, i.e. a read.
const DCTRL_DTDIR_FROM_CARD: u32 = 1 << 1;
const DCTRL_DBLOCKSIZE_SHIFT: u32 = 4;

const STA_CCRCFAIL: u32 = 1 << 0;
const STA_DCRCFAIL: u32 = 1 << 1;
const STA_CTIMEOUT: u32 = 1 << 2;
const STA_DTIMEOUT: u32 = 1 << 3;
const STA_TXUNDERR: u32 = 1 << 4;
const STA_RXOVERR: u32 = 1 << 5;
const STA_CMDREND: u32 = 1 << 6;
const STA_CMDSENT: u32 = 1 << 7;
const STA_DATAEND: u32 = 1 << 8;
const STA_TXFIFOHE: u32 = 1 << 14;
const STA_RXFIFOHF: u32 = 1 << 15;
const STA_RXDAVL: u32 = 1 << 21;

/// Every static flag [`ICR`] can clear -- bits 0..=10 and the SDIO interrupt at 22.
///
/// Cleared before every command, because these are LATCHED: a `CTIMEOUT` left over from the CMD8
/// probe on a v1 card would otherwise be read as a timeout on the next command, and the ladder
/// would fail one step after the one that actually set the bit.
const ICR_CLEAR_ALL: u32 = 0x0040_07FF;

/// How many polls a command or data phase gets before the driver gives up on the hardware.
///
/// This bounds a HOST fault -- a peripheral that never raises a flag because its clock is off or
/// its pins are not in the right alternate function. Card-side timeouts are the controller's own
/// `CTIMEOUT`/`DTIMEOUT` and arrive as flags long before this runs out.
const POLL_LIMIT: u32 = 1_000_000;

/// The value written to `SDMMC_DTIMER`, in CARD CLOCK cycles.
///
/// Generous but BOUNDED: about 0.7 s at 24 MHz. It was `0xFFFF_FFFF` for one board run, which is
/// three minutes -- long enough that the controller's own timeout stops being a safety net at all,
/// and a data phase that never starts looks exactly like a hang.
const DATA_TIMEOUT_CYCLES: u32 = 0x00FF_FFFF;

/// `SDMMC_DCTRL` bit 3. Every FIFO word is fetched by a DMA request instead of by the CPU.
const DCTRL_DMAEN: u32 = 1 << 3;

/// `DMA_LISR` / `DMA_HISR`: streams 0-3 and 4-7. RM0410 8.5.1, 8.5.2.
const DMA_LISR: u32 = 0x00;
/// `DMA_LIFCR` / `DMA_HIFCR`, the write-1-to-clear halves. RM0410 8.5.3, 8.5.4.
const DMA_LIFCR: u32 = 0x08;
/// `DMA_SxCR`, `0x010 + 0x18 * x`. RM0410 8.5.5.
const DMA_SCR: u32 = 0x010;
/// `DMA_SxNDTR`, `0x014 + 0x18 * x`. RM0410 8.5.6.
const DMA_SNDTR: u32 = 0x014;
/// `DMA_SxPAR`, `0x018 + 0x18 * x`. RM0410 8.5.7.
const DMA_SPAR: u32 = 0x018;
/// `DMA_SxM0AR`, `0x01C + 0x18 * x`. RM0410 8.5.8.
const DMA_SM0AR: u32 = 0x01C;
/// `DMA_SxFCR`, `0x024 + 0x18 * x`. RM0410 8.5.10.
const DMA_SFCR: u32 = 0x024;
/// The distance between one stream's registers and the next.
const DMA_STREAM_STRIDE: u32 = 0x18;

/// Where stream `x % 4`'s flags start inside `LISR`/`HISR`. RM0410 8.5.1: the flags sit at bits
/// 27/21/11/5 (`TCIF`), 25/19/9/3 (`TEIF`) and so on, which is four slots at these offsets.
const DMA_FLAG_SLOT: [u32; 4] = [0, 6, 16, 22];
/// Within a slot: `FEIF` +0, `DMEIF` +2, `TEIF` +3, `HTIF` +4, `TCIF` +5.
const DMA_FLAGS_IN_SLOT: u32 = 0b0011_1101;

const DMA_SCR_CHSEL_SHIFT: u32 = 25;
/// `MBURST`/`PBURST` = 01, an incremental burst of four beats. RM0410 8.5.5.
const DMA_SCR_BURST_INCR4: u32 = 0b01;
const DMA_SCR_MBURST_SHIFT: u32 = 23;
const DMA_SCR_PBURST_SHIFT: u32 = 21;
/// `PL` = 11, very high. A card read that loses its arbitration slot becomes an overrun.
const DMA_SCR_PL_VERY_HIGH: u32 = 0b11 << 16;
/// `MSIZE`/`PSIZE` = 10, a 32-bit word -- which is the only width the SDMMC FIFO has.
const DMA_SCR_SIZE_WORD: u32 = 0b10;
const DMA_SCR_MSIZE_SHIFT: u32 = 13;
const DMA_SCR_PSIZE_SHIFT: u32 = 11;
const DMA_SCR_MINC: u32 = 1 << 10;
/// `DIR` = 00, peripheral to memory. Named rather than left implicit in a zero.
const DMA_SCR_DIR_PERIPHERAL_TO_MEMORY: u32 = 0b00 << 6;
/// `DIR` = 01, memory to peripheral -- a card WRITE.
const DMA_SCR_DIR_MEMORY_TO_PERIPHERAL: u32 = 0b01 << 6;
/// `PFCTRL` bit 5: **the PERIPHERAL is the flow controller**, which is what an SD read needs --
/// the controller knows when the card has finished a block and the DMA does not.
const DMA_SCR_PFCTRL: u32 = 1 << 5;
const DMA_SCR_EN: u32 = 1 << 0;
/// `FCR`: `DMDIS` (bit 2) with `FTH` = 11, i.e. direct mode off and a full-FIFO threshold, which is
/// what makes the INCR4 bursts above possible at all.
const DMA_SFCR_DMDIS_FULL: u32 = (1 << 2) | 0b11;

/// How many read-backs of `SDMMC_DCTRL` cover its settling window before it may be written again.
///
/// RM0410 39.8.9 forbids a second write for three SDMMCCLK periods plus two PCLK2 periods. Each
/// read below is one APB round trip, which is at least two PCLK2 periods, so eight of them clear
/// the window on any bridge slower than the core -- and every bridge is.
const DCTRL_SETTLE_READS: u32 = 8;

/// How many turns the FIFO drain gets before it gives up.
///
/// **The loop that moves data needs its own bound and does not inherit one.** `poll` is bounded
/// and the controller's `DTIMER` is bounded, but a drain that spins while no flag changes is
/// answerable to neither.
const DRAIN_LIMIT: u32 = 4_000_000;

/// A failure the controller itself reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stm32SdmmcError {
    /// The peripheral never raised a completion flag. A host fault, not a card one -- see
    /// [`POLL_LIMIT`].
    PeripheralStuck,
    /// The data path reported a CRC failure on a block.
    DataCrc,
    /// The data path timed out waiting for the card.
    DataTimeout,
    /// The receive FIFO overran, i.e. this driver did not drain it fast enough.
    Overrun,
    /// The transmit FIFO underran.
    Underrun,
    /// A transfer length this controller cannot express as whole blocks.
    BadTransferLength,
    /// The DMA stream reported a transfer or direct-mode error.
    DmaFailed,
}

/// One DMA stream, named by the three numbers the chip fixes for it.
///
/// **The stream and channel are CHIP TRUTH and this type does not know them.** Which stream serves
/// which peripheral is a request-mapping table, so the caller states it the same way it already
/// states the controller's base address -- and a wrong pairing is harmless rather than dangerous,
/// because a stream only moves data when its peripheral raises a request. It simply never fires.
///
/// The caller has also already enabled the controller's clock; this type drives one register block
/// and nothing else, exactly like [`Stm32Sdmmc`].
#[derive(Debug, Clone, Copy)]
pub struct DmaStream {
    base: u32,
    stream: u8,
    channel: u8,
}

impl DmaStream {
    /// The stream at `stream` on the controller at `base`, carrying request `channel`.
    #[must_use]
    pub fn new(base: u32, stream: u8, channel: u8) -> Self {
        DmaStream { base, stream: stream & 0b111, channel: channel & 0b1111 }
    }

    fn reg(&self, offset: u32) -> u32 {
        self.base + offset + DMA_STREAM_STRIDE * u32::from(self.stream)
    }

    /// `LISR`/`LIFCR` for streams 0-3, `HISR`/`HIFCR` for 4-7 -- the pair is four bytes apart.
    fn flag_reg(&self, low: u32) -> u32 {
        self.base + low + if self.stream >= 4 { 4 } else { 0 }
    }

    fn slot(&self) -> u32 {
        DMA_FLAG_SLOT[usize::from(self.stream) % 4]
    }

    /// Clears every latched flag for this stream, so the next transfer's are its own.
    fn clear_flags(&self) {
        write32(self.flag_reg(DMA_LIFCR), DMA_FLAGS_IN_SLOT << self.slot());
    }

    /// True if this stream latched a transfer, FIFO or direct-mode error.
    fn errored(&self) -> bool {
        let status = read32(self.flag_reg(DMA_LISR)) >> self.slot();
        status & ((1 << 2) | (1 << 3)) != 0
    }

    /// Stops the stream and waits for the hardware to agree that it has stopped.
    ///
    /// **`EN` is not cleared instantly.** It reads back as 1 until any transfer in flight has
    /// drained, and reconfiguring a stream that is still running is how a stream ends up serving
    /// the previous transfer's addresses.
    fn disable(&self) {
        write32(self.reg(DMA_SCR), read32(self.reg(DMA_SCR)) & !DMA_SCR_EN);
        for _ in 0..POLL_LIMIT {
            if read32(self.reg(DMA_SCR)) & DMA_SCR_EN == 0 {
                break;
            }
        }
        self.clear_flags();
    }

    /// Points the stream at `peripheral`, to fill `buf`, and starts it.
    fn arm_from_peripheral(&self, peripheral: u32, buf: &mut [u8]) {
        self.arm(peripheral, buf.as_mut_ptr() as u32, buf.len(), false);
    }

    /// Points the stream at `peripheral`, to send `buf`, and starts it.
    fn arm_to_peripheral(&self, peripheral: u32, buf: &[u8]) {
        self.arm(peripheral, buf.as_ptr() as u32, buf.len(), true);
    }

    /// Configures and enables the stream in whichever direction.
    ///
    /// The peripheral is left as the flow controller either way, so the transfer ends when the
    /// SDMMC says it does rather than when a count runs out.
    fn arm(&self, peripheral: u32, memory: u32, len: usize, to_peripheral: bool) {
        self.disable();
        write32(self.reg(DMA_SPAR), peripheral);
        write32(self.reg(DMA_SM0AR), memory);
        write32(self.reg(DMA_SNDTR), (len / 4) as u32);
        write32(self.reg(DMA_SFCR), DMA_SFCR_DMDIS_FULL);
        let direction = if to_peripheral {
            DMA_SCR_DIR_MEMORY_TO_PERIPHERAL
        } else {
            DMA_SCR_DIR_PERIPHERAL_TO_MEMORY
        };
        write32(
            self.reg(DMA_SCR),
            (u32::from(self.channel) << DMA_SCR_CHSEL_SHIFT)
                | (DMA_SCR_BURST_INCR4 << DMA_SCR_MBURST_SHIFT)
                | (DMA_SCR_BURST_INCR4 << DMA_SCR_PBURST_SHIFT)
                | DMA_SCR_PL_VERY_HIGH
                | (DMA_SCR_SIZE_WORD << DMA_SCR_MSIZE_SHIFT)
                | (DMA_SCR_SIZE_WORD << DMA_SCR_PSIZE_SHIFT)
                | DMA_SCR_MINC
                | direction
                | DMA_SCR_PFCTRL
                | DMA_SCR_EN,
        );
    }
}

/// One STM32 SDMMC controller.
#[derive(Debug)]
pub struct Stm32Sdmmc {
    base: u32,
    /// The `SDMMCCLK` feeding the divider, which is what turns a requested rate into a `CLKDIV`.
    kernel_clock_hz: u32,
    /// A board-supplied millisecond delay. A function pointer rather than a generic so this type
    /// stays simple to name in a firmware that holds one.
    delay_ms: fn(u32),
    width: BusWidth,
    /// The stream reads use when one has been attached, and `None` for the polled drain.
    dma: Option<DmaStream>,
}

impl Stm32Sdmmc {
    /// A controller at `base`, whose divider is fed by `kernel_clock_hz`.
    ///
    /// **The caller has already enabled the peripheral clock and put the pins into their SDMMC
    /// alternate function.** This crate drives one register block and deliberately knows nothing
    /// about RCC or GPIO -- which port a controller's lines come out on is a BOARD fact, and
    /// baking one board's answer in here is how a driver stops being reusable.
    #[must_use]
    pub fn new(base: u32, kernel_clock_hz: u32, delay_ms: fn(u32)) -> Self {
        Stm32Sdmmc { base, kernel_clock_hz, delay_ms, width: BusWidth::One, dma: None }
    }

    /// Powers the card on and starts its clock, with hardware flow control enabled.
    ///
    /// Flow control is set here rather than left to the caller because a polled driver cannot run
    /// at a useful clock without it: see [`CLKCR_HWFC_EN`]. It survives every later write to this
    /// register, both of which read-modify-write.
    pub fn power_on(&mut self) {
        write32(self.base + POWER, PWRCTRL_ON);
        let clkcr = read32(self.base + CLKCR) | CLKCR_CLKEN | CLKCR_HWFC_EN;
        write32(self.base + CLKCR, clkcr);
    }

    /// Stops the card clock and powers it down.
    pub fn power_off(&mut self) {
        write32(self.base + CLKCR, 0);
        write32(self.base + POWER, 0);
    }

    /// Spins until `done` is set in `SDMMC_STA`, or an `error` bit appears, or the budget runs out.
    fn poll(&self, done: u32, errors: u32) -> Result<u32, SdmmcError<Stm32SdmmcError>> {
        for _ in 0..POLL_LIMIT {
            let status = read32(self.base + STA);
            if status & errors != 0 || status & done != 0 {
                return Ok(status);
            }
        }
        Err(SdmmcError::Host(Stm32SdmmcError::PeripheralStuck))
    }

    /// Issues a command and decodes its response, leaving the data path alone.
    fn issue(
        &mut self,
        index: u8,
        arg: u32,
        kind: ResponseKind,
    ) -> Result<Response, SdmmcError<Stm32SdmmcError>> {
        write32(self.base + ICR, ICR_CLEAR_ALL);
        write32(self.base + ARG, arg);
        let waitresp = match kind {
            ResponseKind::None => CMD_WAITRESP_NONE,
            ResponseKind::Short | ResponseKind::ShortNoCrc => CMD_WAITRESP_SHORT,
            ResponseKind::Long => CMD_WAITRESP_LONG,
        };
        write32(self.base + CMD, u32::from(index) | waitresp | CMD_CPSMEN);

        if kind == ResponseKind::None {
            self.poll(STA_CMDSENT, STA_CTIMEOUT)?;
            let status = read32(self.base + STA);
            write32(self.base + ICR, ICR_CLEAR_ALL);
            return if status & STA_CTIMEOUT != 0 {
                Err(SdmmcError::NoResponse)
            } else {
                Ok(Response::None)
            };
        }

        let status = self.poll(STA_CMDREND, STA_CTIMEOUT | STA_CCRCFAIL)?;
        write32(self.base + ICR, ICR_CLEAR_ALL);

        if status & STA_CTIMEOUT != 0 {
            return Err(SdmmcError::NoResponse);
        }
        if status & STA_CCRCFAIL != 0 && kind != ResponseKind::ShortNoCrc {
            return Err(SdmmcError::BadCrc);
        }

        Ok(match kind {
            ResponseKind::Long => Response::Long([
                read32(self.base + RESP1),
                read32(self.base + RESP1 + 4),
                read32(self.base + RESP1 + 8),
                read32(self.base + RESP1 + 12),
            ]),
            _ => Response::Short(read32(self.base + RESP1)),
        })
    }

    /// Programs the data path for a transfer of `len` bytes in blocks of `block_len`.
    ///
    /// **Called BEFORE the command that starts the transfer.** The card begins driving its data
    /// lines as soon as it has decoded a read command, and a DPSM that is not yet enabled misses
    /// the leading bytes -- which surfaces as a data timeout rather than as anything naming the
    /// real mistake.
    fn arm_data(
        &mut self,
        len: usize,
        block_len: usize,
        from_card: bool,
        dma: bool,
    ) -> Result<(), SdmmcError<Stm32SdmmcError>> {
        let exponent = block_len.trailing_zeros();
        if !block_len.is_power_of_two() || len == 0 || len % block_len != 0 {
            return Err(SdmmcError::Host(Stm32SdmmcError::BadTransferLength));
        }
        self.disarm_data();
        write32(self.base + DTIMER, DATA_TIMEOUT_CYCLES);
        write32(self.base + DLEN, len as u32);
        let direction = if from_card { DCTRL_DTDIR_FROM_CARD } else { 0 };
        let requests = if dma { DCTRL_DMAEN } else { 0 };
        write32(
            self.base + DCTRL,
            (exponent << DCTRL_DBLOCKSIZE_SHIFT) | direction | requests | DCTRL_DTEN,
        );
        Ok(())
    }

    /// Returns the data path to its idle state, so the next transfer does not inherit this one's.
    ///
    /// `DTEN` is not self-clearing and a failed transfer leaves the DPSM armed, so a driver that
    /// never clears it pays for one failure twice: the operation that failed, and the one after
    /// it, which is the one the caller sees.
    ///
    /// **This is a timed write, and the timing is the whole subtlety.** RM0410 39.8.9 ends the
    /// `SDMMC_DCTRL` description with: *"After a data write, data cannot be written to this
    /// register for three SDMMCCLK clock periods plus two PCLK2 clock periods."* Clearing the
    /// register immediately before programming it therefore loses the second write rather than
    /// the first, and a lost arming write is indistinguishable from a card that sent nothing.
    fn disarm_data(&mut self) {
        if read32(self.base + DCTRL) & DCTRL_DTEN == 0 {
            return;
        }
        write32(self.base + DCTRL, 0);
        for _ in 0..DCTRL_SETTLE_READS {
            let _ = read32(self.base + DCTRL);
        }
    }

    /// Reads through `stream` instead of through the CPU, from here on.
    ///
    /// **The caller has already enabled the DMA controller's clock and knows which stream and
    /// channel serve this peripheral.** Both are chip facts this crate has no table for, and a
    /// wrong pairing fails safe: the stream is never asked for anything and the transfer times out.
    ///
    /// Reads whose buffer is not word-aligned still take the polled path, silently and correctly --
    /// see [`read_blocks`](SdmmcHost::read_blocks).
    pub fn attach_dma(&mut self, stream: DmaStream) {
        self.dma = Some(stream);
    }

    /// Goes back to the polled drain, so one board can measure both.
    pub fn detach_dma(&mut self) {
        self.dma = None;
    }

    /// Whether a stream is attached.
    #[must_use]
    pub fn has_dma(&self) -> bool {
        self.dma.is_some()
    }

    /// A read whose FIFO is drained by a DMA stream rather than by this loop.
    ///
    /// The shape is the polled read's with the drain removed: arm the stream, arm the data path
    /// with `DMAEN`, issue the command, and then wait on the SDMMC's own `DATAEND` -- because with
    /// the peripheral as flow controller it is the controller, not the stream, that knows when the
    /// card has finished.
    fn read_blocks_through_dma(
        &mut self,
        stream: DmaStream,
        index: u8,
        arg: u32,
        buf: &mut [u8],
    ) -> Result<(), SdmmcError<Stm32SdmmcError>> {
        let block_len = Self::block_len(index, buf.len());
        stream.arm_from_peripheral(self.base + FIFO, buf);
        let outcome = self.await_dma_read(stream, index, arg, buf.len(), block_len);
        let dma_failed = stream.errored();
        stream.disable();
        self.disarm_data();
        write32(self.base + ICR, ICR_CLEAR_ALL);
        outcome?;
        if dma_failed {
            return Err(SdmmcError::Host(Stm32SdmmcError::DmaFailed));
        }
        Ok(())
    }

    /// A write whose FIFO is filled by a DMA stream rather than by this loop.
    ///
    /// **The command comes FIRST and the data path second**, the mirror of the read and for the
    /// same reason: on a write it is the CARD that would otherwise miss the start. The stream is
    /// armed between them, which is safe because an un-enabled DPSM raises no requests.
    fn write_blocks_through_dma(
        &mut self,
        stream: DmaStream,
        index: u8,
        arg: u32,
        buf: &[u8],
    ) -> Result<(), SdmmcError<Stm32SdmmcError>> {
        let block_len = Self::block_len(index, buf.len());
        let outcome = self.await_dma_write(stream, index, arg, buf, block_len);
        let dma_failed = stream.errored();
        stream.disable();
        self.disarm_data();
        write32(self.base + ICR, ICR_CLEAR_ALL);
        outcome?;
        if dma_failed {
            return Err(SdmmcError::Host(Stm32SdmmcError::DmaFailed));
        }
        Ok(())
    }

    /// The half of a DMA write that can fail, split out so its caller always tears the stream down.
    fn await_dma_write(
        &mut self,
        stream: DmaStream,
        index: u8,
        arg: u32,
        buf: &[u8],
        block_len: usize,
    ) -> Result<(), SdmmcError<Stm32SdmmcError>> {
        self.issue(index, arg, ResponseKind::Short)?;
        stream.arm_to_peripheral(self.base + FIFO, buf);
        self.arm_data(buf.len(), block_len, false, true)?;
        let status = self.poll(STA_DATAEND, STA_DCRCFAIL | STA_DTIMEOUT | STA_TXUNDERR)?;
        Self::data_result(status)
    }

    /// The half of a DMA read that can fail, split out so its caller always tears the stream down.
    fn await_dma_read(
        &mut self,
        _stream: DmaStream,
        index: u8,
        arg: u32,
        len: usize,
        block_len: usize,
    ) -> Result<(), SdmmcError<Stm32SdmmcError>> {
        self.arm_data(len, block_len, true, true)?;
        self.issue(index, arg, ResponseKind::Short)?;
        let status = self.poll(STA_DATAEND, STA_DCRCFAIL | STA_DTIMEOUT | STA_RXOVERR)?;
        Self::data_result(status)
    }

    /// The block size a transfer of this command uses.
    ///
    /// Everything moves 512-byte sectors except the `CMD6` function status, which is a single
    /// 64-byte block. Programming 512 for it would leave the controller waiting for bytes the card
    /// never sends.
    fn block_len(index: u8, len: usize) -> usize {
        if index == lamella_sd_core::cmd::SWITCH_FUNC { len } else { SECTOR_LEN }
    }

    /// Turns the data-phase status bits into an error, or `Ok`.
    fn data_result(status: u32) -> Result<(), SdmmcError<Stm32SdmmcError>> {
        if status & STA_DCRCFAIL != 0 {
            Err(SdmmcError::Host(Stm32SdmmcError::DataCrc))
        } else if status & STA_DTIMEOUT != 0 {
            Err(SdmmcError::Host(Stm32SdmmcError::DataTimeout))
        } else if status & STA_RXOVERR != 0 {
            Err(SdmmcError::Host(Stm32SdmmcError::Overrun))
        } else if status & STA_TXUNDERR != 0 {
            Err(SdmmcError::Host(Stm32SdmmcError::Underrun))
        } else {
            Ok(())
        }
    }
}

impl SdmmcHost for Stm32Sdmmc {
    type Error = Stm32SdmmcError;

    fn command(
        &mut self,
        index: u8,
        arg: u32,
        kind: ResponseKind,
    ) -> Result<Response, SdmmcError<Stm32SdmmcError>> {
        self.issue(index, arg, kind)
    }

    fn read_blocks(
        &mut self,
        index: u8,
        arg: u32,
        buf: &mut [u8],
    ) -> Result<(), SdmmcError<Stm32SdmmcError>> {
        if let Some(stream) = self.dma {
            if buf.as_ptr() as usize % 4 == 0 && buf.len() % 4 == 0 {
                return self.read_blocks_through_dma(stream, index, arg, buf);
            }
        }

        let block_len = Self::block_len(index, buf.len());
        self.arm_data(buf.len(), block_len, true, false)?;
        self.issue(index, arg, ResponseKind::Short)?;

        let errors = STA_DCRCFAIL | STA_DTIMEOUT | STA_RXOVERR;
        let mut written = 0usize;
        let mut turns = 0u32;
        while written < buf.len() {
            turns += 1;
            if turns > DRAIN_LIMIT {
                return Err(SdmmcError::Host(Stm32SdmmcError::DataTimeout));
            }
            let status = read32(self.base + STA);
            Self::data_result(status)?;
            if status & STA_RXFIFOHF != 0 && buf.len() - written >= 32 {
                for _ in 0..8 {
                    let word = read32(self.base + FIFO);
                    buf[written..written + 4].copy_from_slice(&word.to_le_bytes());
                    written += 4;
                }
            } else if status & STA_RXDAVL != 0 {
                let word = read32(self.base + FIFO);
                let take = (buf.len() - written).min(4);
                buf[written..written + take].copy_from_slice(&word.to_le_bytes()[..take]);
                written += take;
            } else if status & STA_DATAEND != 0 {
                break;
            }
        }

        let status = self.poll(STA_DATAEND, errors)?;
        Self::data_result(status)?;
        write32(self.base + ICR, ICR_CLEAR_ALL);
        Ok(())
    }

    fn write_blocks(
        &mut self,
        index: u8,
        arg: u32,
        buf: &[u8],
    ) -> Result<(), SdmmcError<Stm32SdmmcError>> {
        if let Some(stream) = self.dma {
            if buf.as_ptr() as usize % 4 == 0 && buf.len() % 4 == 0 {
                return self.write_blocks_through_dma(stream, index, arg, buf);
            }
        }

        let block_len = Self::block_len(index, buf.len());
        self.issue(index, arg, ResponseKind::Short)?;
        self.arm_data(buf.len(), block_len, false, false)?;

        let errors = STA_DCRCFAIL | STA_DTIMEOUT | STA_TXUNDERR;
        let mut sent = 0usize;
        let mut turns = 0u32;
        while sent < buf.len() {
            turns += 1;
            if turns > DRAIN_LIMIT {
                return Err(SdmmcError::Host(Stm32SdmmcError::DataTimeout));
            }
            let status = read32(self.base + STA);
            Self::data_result(status)?;
            if status & STA_TXFIFOHE != 0 {
                for _ in 0..8 {
                    if sent >= buf.len() {
                        break;
                    }
                    let mut word = [0u8; 4];
                    let take = (buf.len() - sent).min(4);
                    word[..take].copy_from_slice(&buf[sent..sent + take]);
                    write32(self.base + FIFO, u32::from_le_bytes(word));
                    sent += take;
                }
            }
        }

        let status = self.poll(STA_DATAEND, errors)?;
        Self::data_result(status)?;
        write32(self.base + ICR, ICR_CLEAR_ALL);
        Ok(())
    }

    fn set_bus_width(&mut self, width: BusWidth) {
        let bits = match width {
            BusWidth::One => WIDBUS_1BIT,
            BusWidth::Four => WIDBUS_4BIT,
        };
        let clkcr = (read32(self.base + CLKCR) & !CLKCR_WIDBUS_MASK) | bits;
        write32(self.base + CLKCR, clkcr);
        self.width = width;
    }

    fn set_clock_hz(&mut self, hz: u32) {
        let mut clkcr = read32(self.base + CLKCR) & !(CLKCR_CLKDIV_MASK | CLKCR_BYPASS);
        if hz >= self.kernel_clock_hz {
            clkcr |= CLKCR_BYPASS;
        } else {
            let div = self.kernel_clock_hz.div_ceil(hz).saturating_sub(2).min(CLKCR_CLKDIV_MASK);
            clkcr |= div;
        }
        write32(self.base + CLKCR, clkcr | CLKCR_CLKEN);
    }

    fn delay_ms(&mut self, ms: u32) {
        (self.delay_ms)(ms);
    }
}

#[cfg(test)]
mod tests {
    use super::*;


    fn divider_for(kernel_hz: u32, requested: u32) -> u32 {
        if requested >= kernel_hz {
            return 0;
        }
        kernel_hz.div_ceil(requested).saturating_sub(2).min(0xFF)
    }

    /// The rate a divider actually produces, per RM0410 39.8.2.
    fn produced(kernel_hz: u32, div: u32) -> u32 {
        kernel_hz / (div + 2)
    }

    #[test]
    fn the_divider_never_produces_a_rate_above_the_one_requested() {
        let kernel = 48_000_000;
        for requested in [400_000, 1_000_000, 12_000_000, 20_000_000, 24_000_000, 25_000_000] {
            let rate = produced(kernel, divider_for(kernel, requested));
            assert!(rate <= requested, "asked {requested}, produced {rate}");
        }
    }

    #[test]
    fn the_identification_rate_lands_inside_the_four_hundred_kilohertz_band() {
        let rate = produced(48_000_000, divider_for(48_000_000, 400_000));
        assert!(rate <= 400_000, "{rate} exceeds the identification ceiling");
        assert!(rate > 100_000, "{rate} is below the band and would be needlessly slow");
    }

    #[test]
    fn default_speed_lands_at_twenty_four_megahertz_on_a_forty_eight_megahertz_kernel() {
        assert_eq!(produced(48_000_000, divider_for(48_000_000, 25_000_000)), 24_000_000);
    }

    #[test]
    fn a_rate_at_or_above_the_kernel_clock_selects_bypass_rather_than_a_divider() {
        assert_eq!(divider_for(48_000_000, 50_000_000), 0, "high speed is above the kernel clock");
    }

    #[test]
    fn the_block_size_exponent_matches_the_two_sizes_this_driver_moves() {
        assert_eq!(SECTOR_LEN.trailing_zeros(), 9);
        assert_eq!(64usize.trailing_zeros(), 6);
    }

    #[test]
    fn the_switch_status_uses_its_own_block_length_and_sectors_use_the_sector_size() {
        assert_eq!(Stm32Sdmmc::block_len(lamella_sd_core::cmd::SWITCH_FUNC, 64), 64);
        assert_eq!(Stm32Sdmmc::block_len(lamella_sd_core::cmd::READ_SINGLE_BLOCK, 2048), SECTOR_LEN);
    }
}
