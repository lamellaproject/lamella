// Lamella managed corlib (from scratch). -- System.MidpointRounding
#if LAMELLA_SURFACE_NETFX_2_0
namespace System
{
    /// <summary>Says how a rounding method treats a value that lies exactly halfway between two
    /// candidates, or, for the directed modes, which way every value rounds.</summary>
    public enum MidpointRounding
    {
        /// <summary>A value halfway between two candidates rounds to the even one: 2.5 rounds to 2 and
        /// 3.5 to 4.</summary>
        ToEven = 0,

        /// <summary>A value halfway between two candidates rounds away from zero: 2.5 rounds to 3 and
        /// -2.5 to -3.</summary>
        AwayFromZero = 1,
#if LAMELLA_SURFACE_NETCORE_3_0

        /// <summary>Every value rounds toward zero, as truncation does: 2.8 rounds to 2 and -2.8 to
        /// -2.</summary>
        ToZero = 2,

        /// <summary>Every value rounds down, toward negative infinity: 2.8 rounds to 2 and -2.2 to
        /// -3.</summary>
        ToNegativeInfinity = 3,

        /// <summary>Every value rounds up, toward positive infinity: 2.2 rounds to 3 and -2.8 to
        /// -2.</summary>
        ToPositiveInfinity = 4,
#endif
    }
}
#endif
