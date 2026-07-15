// Lamella System.Device.Spi -- the dotnet/iot SPI API, in the System.Device.Gpio assembly.
namespace System.Device.Spi
{
    /// <summary>Base class for SPI drivers: the chip-level bus primitives a board or chip
    /// implementation provides, and the seam <see cref="SpiDevice"/> sits on.</summary>
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
        /// <paramref name="readBuffer"/>, as ONE bus operation. A null
        /// <paramref name="writeBuffer"/> clocks zeros out; a null
        /// <paramref name="readBuffer"/> discards the inbound words. Returns 0 on success or
        /// a nonzero chip status.</summary>
        public abstract int TransferFullDuplex(byte[] writeBuffer, byte[] readBuffer, int count);

        /// <summary>Asserts or releases the managed chip select
        /// (<paramref name="asserted"/> is logical: the driver maps it to the pin level via
        /// <see cref="SpiConnectionSettings.ChipSelectLineActiveState"/>). A no-op when the
        /// bus runs raw or the chip select is hardware-framed.</summary>
        public abstract void SetChipSelect(bool asserted);

        /// <summary>The clock frequency, in hertz, the chip actually realized for the
        /// requested settings (a divided clock rarely lands exactly on the request).</summary>
        public abstract int ActualClockFrequency { get; }

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
