// Lamella managed corlib (from scratch). -- System.MidpointRounding
#if LAMELLA_SURFACE_NETFX_2_0
namespace System
{
    /// <summary>Says how a rounding method treats a value that lies exactly halfway between two
    /// candidates.</summary>
    public enum MidpointRounding
    {
        /// <summary>A value halfway between two candidates rounds to the even one: 2.5 rounds to 2 and
        /// 3.5 to 4.</summary>
        ToEven = 0,

        /// <summary>A value halfway between two candidates rounds away from zero: 2.5 rounds to 3 and
        /// -2.5 to -3.</summary>
        AwayFromZero = 1,
    }
}
#endif
