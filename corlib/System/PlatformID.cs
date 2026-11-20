// Lamella managed corlib (from scratch). -- System.PlatformID
namespace System
{
    /// <summary>The operating system family an <see cref="OperatingSystem"/> names.</summary>
    public enum PlatformID
    {
        /// <summary>Win32S, a layer that ran 32-bit programs on 16-bit Windows.</summary>
        Win32S = 0,
        /// <summary>Windows 95 or Windows 98.</summary>
        Win32Windows = 1,
        /// <summary>Windows NT and its successors.</summary>
        Win32NT = 2,
#if LAMELLA_SURFACE_NETFX_1_1
        /// <summary>Windows CE.</summary>
        WinCE = 3,
#endif
#if LAMELLA_SURFACE_NETFX_2_0
        /// <summary>Unix.</summary>
        Unix = 4,
        /// <summary>The Xbox 360.</summary>
        Xbox = 5,
        /// <summary>macOS.</summary>
        MacOSX = 6,
#endif
#if LAMELLA_SURFACE_NET_5_0
        /// <summary>An operating system the other members do not name; a Lamella board answers this.</summary>
        Other = 7,
#endif
    }
}
