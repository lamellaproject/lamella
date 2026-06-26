// Lamella managed corlib (from scratch). -- System.Net.IPEndPoint
#if LAMELLA_SURFACE_NET
namespace System.Net
{
    public class IPEndPoint : EndPoint
    {
        private IPAddress _address;
        private int _port;

        public IPEndPoint(IPAddress address, int port)
        {
            _address = address;
            _port = port;
        }

        public IPAddress Address { get { return _address; } }

        public int Port { get { return _port; } }
    }
}
#endif
