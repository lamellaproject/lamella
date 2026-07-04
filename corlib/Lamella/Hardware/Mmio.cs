// Lamella managed corlib (from scratch). -- Lamella.Hardware.Mmio
#if LAMELLA_SURFACE_MMIO
namespace Lamella.Hardware
{
    /// <summary>Volatile access to memory-mapped hardware registers, for peripheral drivers.</summary>
    public static class Mmio
    {
        /// <summary>Volatile 32-bit read of the register at <paramref name="address"/>.</summary>
        [Lamella.Runtime.RuntimeProvided]
        public static uint Read32(uint address) { return 0; }

        /// <summary>Volatile 32-bit write of <paramref name="value"/> to the register at <paramref name="address"/>.</summary>
        [Lamella.Runtime.RuntimeProvided]
        public static void Write32(uint address, uint value) { }
    }
}
#endif
