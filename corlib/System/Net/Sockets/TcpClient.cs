// Lamella managed corlib (from scratch). -- System.Net.Sockets.TcpClient
#if LAMELLA_SURFACE_NET
namespace System.Net.Sockets
{
    public class TcpClient
    {
        private Socket _socket;
        private NetworkStream _stream;

        public TcpClient()
        {
            _socket = new Socket(AddressFamily.InterNetwork, SocketType.Stream, ProtocolType.Tcp);
        }

        internal TcpClient(Socket acceptedSocket) { _socket = acceptedSocket; }

        public void Connect(IPAddress address, int port)
        {
            _socket.Connect(new IPEndPoint(address, port));
        }

        public void Connect(IPEndPoint remoteEP) { _socket.Connect(remoteEP); }

        public NetworkStream GetStream()
        {
            if (_stream == null) _stream = new NetworkStream(_socket);
            return _stream;
        }

        public void Close() { _socket.Close(); }
    }
}
#endif
