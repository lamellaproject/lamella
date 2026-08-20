// System.IO.Ports (libs/, real .NET's own assembly name) -- System.IO.Ports.Handshake
#if LAMELLA_SURFACE_SERIAL && LAMELLA_SURFACE_NETFX_2_0
namespace System.IO.Ports
{
    public enum Handshake
    {
        None = 0,
        XOnXOff = 1,
        RequestToSend = 2,
        RequestToSendXOnXOff = 3,
    }
}
#endif
