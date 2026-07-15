// Lamella managed corlib (from scratch). -- System.IO.Ports.Parity
#if LAMELLA_SURFACE_SERIAL
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
