// Lamella managed corlib (from scratch). -- System.Net.Sockets.TcpListener
#if LAMELLA_SURFACE_NET
namespace System.Net.Sockets
{
    public class TcpListener
    {
        private Socket _socket;
        private IPEndPoint _endpoint;

        public TcpListener(IPEndPoint localEP)
        {
            _endpoint = localEP;
            _socket = new Socket(AddressFamily.InterNetwork, SocketType.Stream, ProtocolType.Tcp);
        }

        public TcpListener(IPAddress localaddr, int port)
        {
            _endpoint = new IPEndPoint(localaddr, port);
            _socket = new Socket(AddressFamily.InterNetwork, SocketType.Stream, ProtocolType.Tcp);
        }

        public void Start() { Start(16); }

        public void Start(int backlog)
        {
            _socket.Bind(_endpoint);
            _socket.Listen(backlog);
        }

        public Socket AcceptSocket() { return _socket.Accept(); }

        public TcpClient AcceptTcpClient() { return new TcpClient(_socket.Accept()); }

        public EndPoint LocalEndpoint { get { return _socket.LocalEndPoint; } }

        public void Stop() { _socket.Close(); }
    }
}
#endif
