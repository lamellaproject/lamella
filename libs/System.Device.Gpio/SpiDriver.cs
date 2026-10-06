// Lamella.Hardware -- the SPI chip-driver seam, in the System.Device.Gpio assembly.
using System.Device.Spi;

namespace Lamella.Hardware
{
    /// <summary>Base class for SPI drivers: the chip-level bus primitives a board or chip
    /// implementation provides, and the seam <see cref="System.Device.Spi.SpiDevice"/> sits
    /// on.</summary>
    /// <remarks>Every device on a bus shares the bus's one driver. A
    /// <see cref="System.Device.Spi.SpiDevice"/> holds this driver's lock (the one the C#
    /// <c>lock</c> statement takes on it) across each of its operations: applying its settings
    /// with <see cref="Configure"/> when another device's were applied last, asserting its chip
    /// select, the transfer, and releasing the select. Code that drives this driver directly can
    /// take the same lock to keep the bus's devices off it meanwhile.</remarks>
    public abstract class SpiDriver : System.IDisposable
    {
        internal SpiDevice ConfiguredFor;

        /// <summary>Claims the bus's pins (including, as a GPIO the driver drives, the chip select
        /// <see cref="SpiConnectionSettings.ChipSelectLine"/> indexes among the bus's chip selects,
        /// unless it is -1, the value that means no chip select) and applies the settings through
        /// the chip's initialization sequence. Settings outside the chip's envelope (an
        /// unreachable clock, an unsupported word length or bit order) are rejected loudly here
        /// rather than silently degraded.</summary>
        /// <remarks>The line is -1 or one of the <see cref="ChipSelectCount"/> lines of the bus:
        /// <see cref="System.Device.Spi.SpiDevice"/> refuses any other before it calls this. A
        /// driver may be configured again for another device's settings at any time, and the select
        /// a previous configuration claimed stays idle.</remarks>
        public abstract void Configure(SpiConnectionSettings settings);

        /// <summary>How many chip selects the bus has: <see cref="SpiConnectionSettings.ChipSelectLine"/>
        /// 0 to one less than this selects one, and -1 none. 0 unless the driver states its bus's
        /// selects.</summary>
        public virtual int ChipSelectCount
        {
            get { return 0; }
        }

        /// <summary>The pin chip-select line <paramref name="line"/> drives, numbered as the board's
        /// <see cref="System.Device.Gpio.GpioController"/> numbers its pins, for
        /// <paramref name="line"/> 0 to one less than <see cref="ChipSelectCount"/>. A driver that
        /// states a count answers for each of those lines.</summary>
        public virtual int GetChipSelectPin(int line)
        {
            throw new System.ArgumentOutOfRangeException("line");
        }

        /// <summary>Clocks <paramref name="count"/> words out of <paramref name="writeBuffer"/>
        /// while simultaneously clocking <paramref name="count"/> words into
        /// <paramref name="readBuffer"/>, as ONE bus operation. An EMPTY
        /// <paramref name="writeBuffer"/> clocks zeros out; an empty
        /// <paramref name="readBuffer"/> discards the inbound words. Returns 0 on success or
        /// a nonzero chip status.</summary>
        public abstract int TransferFullDuplex(System.ReadOnlySpan<byte> writeBuffer,
                                               System.Span<byte> readBuffer, int count);

        /// <summary>Asserts or releases the managed chip select of the settings applied last
        /// (<paramref name="asserted"/> is logical: the driver maps it to the pin level via
        /// <see cref="SpiConnectionSettings.ChipSelectLineActiveState"/>). A no-op when the
        /// settings name no chip select.</summary>
        public abstract void SetChipSelect(bool asserted);

        /// <summary>The clock frequency, in hertz, the chip actually realized for the
        /// requested settings (a divided clock rarely lands exactly on the request).</summary>
        public abstract int ActualClockFrequency { get; }

        /// <summary>The value by which a native owner names the peripheral instance this driver
        /// drives, or 0 when the driver has no such instance. On a memory-mapped chip it is the
        /// peripheral's register base -- the same base this driver composed its own registers from,
        /// so a native driver built from the chip package's identical base names the identical
        /// instance without either side inventing a bus-naming scheme for the other.</summary>
        public virtual uint NativeBusIdentity
        {
            get { return 0; }
        }

        /// <summary>Disposes this instance.</summary>
        public void Dispose()
        {
            Dispose(true);
        }

        /// <summary>Releases the driver's resources (claimed pins included).</summary>
        protected virtual void Dispose(bool disposing)
        {
        }
    }
}
