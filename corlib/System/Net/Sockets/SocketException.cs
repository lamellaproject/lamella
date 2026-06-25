// Lamella managed corlib (from scratch). -- System.Net.Sockets.SocketException
namespace System.Net.Sockets
{
    public class SocketException : Exception
    {
        public SocketException() : base("A socket operation failed.") { }

        public SocketException(string message) : base(message) { }
    }
}
