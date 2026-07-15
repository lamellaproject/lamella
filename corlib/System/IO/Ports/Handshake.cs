// Lamella managed corlib (from scratch). -- System.IO.Ports.Handshake
#if LAMELLA_SURFACE_SERIAL
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
