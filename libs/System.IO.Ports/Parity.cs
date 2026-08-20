// System.IO.Ports (libs/, real .NET's own assembly name) -- System.IO.Ports.Parity
#if LAMELLA_SURFACE_SERIAL && LAMELLA_SURFACE_NETFX_2_0
namespace System.IO.Ports
{
    public enum Parity
    {
        None = 0,
        Odd = 1,
        Even = 2,
        Mark = 3,
        Space = 4,
    }
}
#endif
