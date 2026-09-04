// Lamella.Hardware -- the SPI chip-driver seam, in the System.Device.Gpio assembly.
using System.Device.Spi;

namespace Lamella.Hardware
{
    /// <summary>Base class for SPI drivers: the chip-level bus primitives a board or chip
    /// implementation provides, and the seam <see cref="System.Device.Spi.SpiDevice"/> sits
    /// on.</summary>
    public abstract class SpiDriver : System.IDisposable
    {
        /// <summary>Claims the bus's pins (including a GPIO-claimed managed chip select when
        /// <see cref="SpiConnectionSettings.ChipSelectLine"/> is not -1) and applies the
        /// settings through the chip's initialization sequence. Settings outside the chip's
        /// envelope (an unreachable clock, an unsupported word length or bit order) are
        /// rejected loudly here rather than silently degraded.</summary>
        public abstract void Configure(SpiConnectionSettings settings);

        /// <summary>Clocks <paramref name="count"/> words out of <paramref name="writeBuffer"/>
        /// while simultaneously clocking <paramref name="count"/> words into
        /// <paramref name="readBuffer"/>, as ONE bus operation. An EMPTY
        /// <paramref name="writeBuffer"/> clocks zeros out; an empty
        /// <paramref name="readBuffer"/> discards the inbound words. Returns 0 on success or
        /// a nonzero chip status.</summary>
        public abstract int TransferFullDuplex(System.ReadOnlySpan<byte> writeBuffer,
                                               System.Span<byte> readBuffer, int count);

        /// <summary>Asserts or releases the managed chip select
        /// (<paramref name="asserted"/> is logical: the driver maps it to the pin level via
        /// <see cref="SpiConnectionSettings.ChipSelectLineActiveState"/>). A no-op when the
        /// bus runs raw or the chip select is hardware-framed.</summary>
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
