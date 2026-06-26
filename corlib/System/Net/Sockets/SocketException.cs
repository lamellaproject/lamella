// Lamella managed corlib (from scratch). -- System.Net.Sockets.SocketException
#if LAMELLA_SURFACE_NET
namespace System.Net.Sockets
{
    public class SocketException : Exception
    {
        public SocketException() : base("A socket operation failed.") { }

        public SocketException(string message) : base(message) { }
    }
}
#endif
