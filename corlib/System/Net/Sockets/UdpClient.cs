// Lamella managed corlib (from scratch). -- System.Net.Sockets.UdpClient
#if LAMELLA_SURFACE_NET
namespace System.Net.Sockets
{
    public class UdpClient
    {
        private Socket _socket;

        private const int MaxDatagram = 65536;

        public UdpClient()
        {
            _socket = new Socket(AddressFamily.InterNetwork, SocketType.Dgram, ProtocolType.Udp);
        }

        public UdpClient(int port)
        {
            _socket = new Socket(AddressFamily.InterNetwork, SocketType.Dgram, ProtocolType.Udp);
            _socket.Bind(new IPEndPoint(IPAddress.Any, port));
        }

        public UdpClient(IPEndPoint localEP)
        {
            if ((object)localEP == null) throw new ArgumentNullException("localEP");
            _socket = new Socket(AddressFamily.InterNetwork, SocketType.Dgram, ProtocolType.Udp);
            _socket.Bind(localEP);
        }

        public Socket Client
        {
            get { return _socket; }
            set { _socket = value; }
        }

        public int Send(byte[] dgram, int bytes, IPEndPoint endPoint)
        {
            if ((object)dgram == null) throw new ArgumentNullException("dgram");
            return _socket.SendTo(dgram, 0, bytes, SocketFlags.None, endPoint);
        }

        public byte[] Receive(ref IPEndPoint remoteEP)
        {
            byte[] buffer = new byte[MaxDatagram];
            EndPoint sender = new IPEndPoint(IPAddress.Any, 0);
            int received = _socket.ReceiveFrom(buffer, ref sender);
            remoteEP = (IPEndPoint)sender;
            byte[] result = new byte[received];
            for (int i = 0; i < received; i++) result[i] = buffer[i];
            return result;
        }

        public void Close() { _socket.Close(); }
    }
}
#endif
