// System.IO.Ports (libs/, real .NET's own assembly name) -- System.IO.Ports.StopBits
#if LAMELLA_SURFACE_SERIAL && LAMELLA_SURFACE_NETFX_2_0
namespace System.IO.Ports
{
    public enum StopBits
    {
        None = 0,
        One = 1,
        Two = 2,
        OnePointFive = 3,
    }
}
#endif
