// Lamella managed corlib (from scratch). -- Lamella.Hardware.Mmio
#if LAMELLA_SURFACE_MMIO
namespace Lamella.Hardware
{
    /// <summary>Volatile access to memory-mapped hardware registers, for peripheral drivers.</summary>
    public sealed class Mmio
    {
        private Mmio() { }

        /// <summary>Volatile 32-bit read of the register at <paramref name="address"/>.</summary>
        [Lamella.Runtime.RuntimeProvided]
        public static uint Read32(uint address) { return 0; }

        /// <summary>Volatile 32-bit write of <paramref name="value"/> to the register at <paramref name="address"/>.</summary>
        [Lamella.Runtime.RuntimeProvided]
        public static void Write32(uint address, uint value) { }

        /// <summary>Volatile 8-bit read of the register at <paramref name="address"/>.</summary>
        [Lamella.Runtime.RuntimeProvided]
        public static byte Read8(uint address) { return 0; }

        /// <summary>Volatile 8-bit write of <paramref name="value"/> to the register at <paramref name="address"/>.</summary>
        [Lamella.Runtime.RuntimeProvided]
        public static void Write8(uint address, byte value) { }

        /// <summary>Volatile 16-bit read of the register at <paramref name="address"/>.</summary>
        [Lamella.Runtime.RuntimeProvided]
        public static ushort Read16(uint address) { return 0; }

        /// <summary>Volatile 16-bit write of <paramref name="value"/> to the register at <paramref name="address"/>.</summary>
        [Lamella.Runtime.RuntimeProvided]
        public static void Write16(uint address, ushort value) { }
    }
}
#endif
