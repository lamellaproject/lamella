// Lamella.Hardware -- the I2C chip-driver seam, in the System.Device.Gpio assembly.
namespace Lamella.Hardware
{
    /// <summary>Base class for I2C drivers: the chip-level bus primitives a board or chip
    /// implementation provides, and the seam <see cref="System.Device.I2c.I2cDevice"/> sits
    /// on.</summary>
    public abstract class I2cDriver : System.IDisposable
    {
        /// <summary>The transfer completed; every byte was acknowledged.</summary>
        public const int Ok = 0;
        /// <summary>No device acknowledged the ADDRESS phase (the normal result of probing
        /// an empty address; chips that name the phase -- an abort source register, separate
        /// address/data NACK flags -- report it distinctly).</summary>
        public const int AddressNack = 1;
        /// <summary>The device acknowledged its address but not a DATA byte.</summary>
        public const int DataNack = 2;
        /// <summary>The transfer failed some other way (arbitration loss, a bus fault); the
        /// chip-specific detail stays readable through the chip's own registers.</summary>
        public const int OtherError = 3;
        /// <summary>The transfer did not finish within the bus's time limit: a device held the
        /// clock low too long, or a line is stuck. The driver abandoned it, so no part of it runs
        /// once the bus moves again.</summary>
        public const int TimedOut = 4;
        /// <summary>The request was refused before anything reached the bus, because the chip's
        /// own datasheet rules it out: an address the controller does not support, or a transfer
        /// it cannot make, such as one of no bytes.</summary>
        public const int InvalidRequest = 5;

        /// <summary>Claims the bus's pins and runs the chip's initialization sequence at
        /// <paramref name="busHz"/>. A rate outside the chip's envelope is rejected loudly.
        /// The bus rate is bus-level state, configured ONCE (the official device settings carry
        /// no frequency): <see cref="Lamella.Hardware.Buses"/> does it at the rate a board bound,
        /// before any device sees the driver, and a driver made outside the table is configured
        /// by whoever made it.</summary>
        public abstract void Configure(int busHz);

        /// <summary>One write transaction: START, address+W, <paramref name="count"/> bytes
        /// of <paramref name="buffer"/>, STOP. A count of zero sends the address alone. Returns
        /// a normalized status.</summary>
        public abstract int Write(int address, System.ReadOnlySpan<byte> buffer, int count);

        /// <summary>One read transaction: START, address+R, <paramref name="count"/> bytes
        /// into <paramref name="buffer"/>, STOP. Returns a normalized status.</summary>
        public abstract int Read(int address, System.Span<byte> buffer, int count);

        /// <summary>One combined transaction with a REPEATED START between the phases:
        /// START, address+W, the write bytes, RESTART, address+R, the read bytes, STOP --
        /// the register-read primitive. Returns a normalized status.</summary>
        public abstract int WriteRead(int address, System.ReadOnlySpan<byte> writeBuffer, int writeCount,
                                      System.Span<byte> readBuffer, int readCount);

        private readonly byte[] _probeScratch = new byte[1];

        /// <summary>Whether a device answers at <paramref name="address"/>, as a normalized
        /// status (the bus-scanner primitive: <see cref="AddressNack"/> on an empty address
        /// is the expected data, not an error). The default probes with a one-byte read into
        /// an instance scratch buffer, so a scan loop allocates nothing; a chip driver
        /// overrides it when the silicon has a cheaper probe (an abort-and-clear cycle, a
        /// quick command).</summary>
        public virtual int Probe(int address)
        {
            return Read(address, new System.Span<byte>(_probeScratch), 1);
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
