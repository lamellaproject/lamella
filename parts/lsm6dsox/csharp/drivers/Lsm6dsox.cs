// A driver for the ST LSM6DSOX six-axis inertial module: find it, configure it, read it.

using Lamella.Generated;
using Lamella.Hardware;

namespace Lamella.Devices
{
    /// <summary>One reading of the accelerometer, in milli-g on each axis.</summary>
    public struct Acceleration
    {
        /// <summary>Milli-g along X.</summary>
        public int X;
        /// <summary>Milli-g along Y.</summary>
        public int Y;
        /// <summary>Milli-g along Z.</summary>
        public int Z;

        /// <summary>The magnitude of the vector, in milli-g.</summary>
        /// <remarks>
        /// THE ONE FIGURE THAT CHECKS ITSELF. A body at rest reads about 1000 here whatever its
        /// orientation, so a caller can tell a working sensor from a plausible-looking wrong one
        /// without a reference instrument and without knowing which way up the part is mounted --
        /// which matters, because a module on a cable has no defined orientation.
        /// </remarks>
        public int MilliG
        {
            get { return IntegerSqrt(X * X + Y * Y + Z * Z); }
        }

        private static int IntegerSqrt(int n)
        {
            if (n <= 0) { return 0; }
            int x = n;
            int y = (x + 1) / 2;
            while (y < x) { x = y; y = (x + n / x) / 2; }
            return x;
        }
    }

    /// <summary>The ST LSM6DSOX, over I2C.</summary>
    public sealed class Lsm6dsox
    {
        /// <summary>The address when the SDO/SA0 pin is tied low.</summary>
        public const int AddressSa0Low = (int)Lsm6dsoxPart.ADDRESS_STRAP_SA0_LOW;

        /// <summary>The address when the SDO/SA0 pin is tied high.</summary>
        public const int AddressSa0High = (int)Lsm6dsoxPart.ADDRESS_STRAP_SA0_HIGH;

        private const byte Ctrl1XlOdr12Hz5Fs2G = (0x1 << 4) | (0x0 << 2);

        private const int NanoGPerLsb = 61000;

        private readonly I2cDriver _bus;
        private readonly int _address;
        private readonly byte[] _one = new byte[1];
        private readonly byte[] _reg = new byte[1];
        private readonly byte[] _frame = new byte[(int)Lsm6dsoxPart.BURST_LENGTH];

        /// <summary>Binds to a part at <paramref name="address"/> on an already-configured bus.</summary>
        /// <remarks>
        /// THE ADDRESS IS REQUIRED RATHER THAN DEFAULTED. The part states two and a carrier picks
        /// one by where it tied a pin, so a default here would be this driver guessing at a fact
        /// only the board knows.
        /// </remarks>
        public Lsm6dsox(I2cDriver bus, int address)
        {
            _bus = bus;
            _address = address;
        }

        /// <summary>Reads the identity register and reports whether it is one this part accepts.</summary>
        /// <remarks>
        /// ON A MISMATCH THE VALUE READ IS RETURNED, not merely a false. A rejected part reads as
        /// no part at all, and the number is what tells a wrong part from an empty address.
        /// </remarks>
        public bool TryIdentify(out byte id)
        {
            id = 0;
            byte value;
            if (!ReadRegister((byte)Lsm6dsoxPart.IDENTITY_REG, out value)) { return false; }
            id = value;
            return value == (byte)Lsm6dsoxPart.IDENTITY_VALUE_0;
        }

        /// <summary>Brings the accelerometer out of power-down at 12.5 Hz, plus or minus 2 g.</summary>
        /// <remarks>
        /// REQUIRED BEFORE ANY READING. The part powers up with its rate field zero, which is
        /// power-down: without this the output registers hold zero and the part reports no error,
        /// so an unconfigured device looks like a stationary one in free fall.
        /// </remarks>
        public bool Configure()
        {
            return WriteRegister((byte)Lsm6dsoxPart.CTRL1_XL_REG, Ctrl1XlOdr12Hz5Fs2G);
        }

        /// <summary>Whether a new accelerometer sample is waiting.</summary>
        public bool AccelerationReady()
        {
            byte status;
            if (!ReadRegister((byte)Lsm6dsoxPart.STATUS_REG_REG, out status)) { return false; }
            return (status & (byte)Lsm6dsoxPart.STATUS_REG_XLDA) != 0;
        }

        /// <summary>Reads one acceleration sample.</summary>
        /// <remarks>
        /// ONE BURST FROM THE ACCELERATION BLOCK, never six single reads: the axes must come from
        /// one conversion, and byte-at-a-time reads can straddle two. The burst also depends on the
        /// interface auto-increment bit, which this part sets at reset -- without it every axis
        /// comes back equal to X, which is a plausible frame rather than a failure.
        /// </remarks>
        public bool ReadAcceleration(out Acceleration reading)
        {
            reading = new Acceleration();
            _reg[0] = (byte)Lsm6dsoxPart.OUTX_L_A_REG;
            int status = _bus.WriteRead(_address, _reg, 1, _frame, _frame.Length);
            if (status != I2cDriver.Ok) { return false; }

            reading.X = ToMilliG(_frame[0], _frame[1]);
            reading.Y = ToMilliG(_frame[2], _frame[3]);
            reading.Z = ToMilliG(_frame[4], _frame[5]);
            return true;
        }

        private static int ToMilliG(byte low, byte high)
        {
            short raw = (short)((high << 8) | low);
            return (raw * NanoGPerLsb) / 1000000;
        }

        private bool ReadRegister(byte register, out byte value)
        {
            value = 0;
            _reg[0] = register;
            int status = _bus.WriteRead(_address, _reg, 1, _one, 1);
            if (status != I2cDriver.Ok) { return false; }
            value = _one[0];
            return true;
        }

        private bool WriteRegister(byte register, byte value)
        {
            byte[] payload = new byte[2];
            payload[0] = register;
            payload[1] = value;
            return _bus.Write(_address, payload, 2) == I2cDriver.Ok;
        }
    }
}
