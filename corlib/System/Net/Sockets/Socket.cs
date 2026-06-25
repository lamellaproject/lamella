// Lamella managed corlib (from scratch). -- System.Net.Sockets.Socket
namespace System.Net.Sockets
{
    public class Socket
    {
        private int _handle;
        private IPEndPoint _bindEndPoint;
        private SocketType _socketType;

        private const int WouldBlock = -1;
        private const int SockError = -2;

        public Socket(AddressFamily addressFamily, SocketType socketType, ProtocolType protocolType)
        {
            _socketType = socketType;
            _handle = -1;
        }

        private Socket(int handle) { _handle = handle; }

        public void Connect(EndPoint remoteEP)
        {
            IPEndPoint endpoint = (IPEndPoint)remoteEP;
            int handle = ConnectStart(endpoint.Address.GetAddressBytes(), endpoint.Port);
            if (handle < 0) throw new SocketException();
            _handle = handle;
            int result;
            while ((result = ConnectPoll(_handle)) == WouldBlock) { }
            if (result == SockError) throw new SocketException();
        }

        public void Bind(EndPoint localEP)
        {
            IPEndPoint endpoint = (IPEndPoint)localEP;
            if (_socketType == SocketType.Dgram)
            {
                int handle = UdpBind(endpoint.Address.GetAddressBytes(), endpoint.Port);
                if (handle < 0) throw new SocketException();
                _handle = handle;
            }
            else
            {
                _bindEndPoint = endpoint;
            }
        }

        public int SendTo(byte[] buffer, int offset, int size, SocketFlags socketFlags, EndPoint remoteEP)
        {
            IPEndPoint endpoint = (IPEndPoint)remoteEP;
            int sent;
            while ((sent = UdpSendTo(_handle, buffer, offset, size, endpoint.Address.GetAddressBytes(), endpoint.Port)) == WouldBlock) { }
            if (sent == SockError) throw new SocketException();
            return sent;
        }

        public int SendTo(byte[] buffer, EndPoint remoteEP)
        {
            return SendTo(buffer, 0, buffer.Length, SocketFlags.None, remoteEP);
        }

        public int ReceiveFrom(byte[] buffer, ref EndPoint remoteEP)
        {
            byte[] senderAddr = new byte[16];
            int[] senderMeta = new int[2];
            int received;
            while ((received = UdpReceiveFrom(_handle, buffer, 0, buffer.Length, senderAddr, senderMeta)) == WouldBlock) { }
            if (received == SockError) throw new SocketException();
            int addrLength = senderMeta[0];
            byte[] address = new byte[addrLength];
            for (int i = 0; i < addrLength; i++) address[i] = senderAddr[i];
            remoteEP = new IPEndPoint(new IPAddress(address), senderMeta[1]);
            return received;
        }

        public void Listen(int backlog)
        {
            int handle = ListenStart(_bindEndPoint.Address.GetAddressBytes(), _bindEndPoint.Port, backlog);
            if (handle < 0) throw new SocketException();
            _handle = handle;
        }

        public Socket Accept()
        {
            int handle;
            while ((handle = AcceptPoll(_handle)) == WouldBlock) { }
            if (handle == SockError) throw new SocketException();
            return new Socket(handle);
        }

        public int Send(byte[] buffer) { return SendCore(buffer, 0, buffer.Length); }

        public int Send(byte[] buffer, int offset, int size, SocketFlags socketFlags)
        {
            return SendCore(buffer, offset, size);
        }

        private int SendCore(byte[] buffer, int offset, int size)
        {
            int sent;
            while ((sent = SendPoll(_handle, buffer, offset, size)) == WouldBlock) { }
            if (sent == SockError) throw new SocketException();
            return sent;
        }

        public int Receive(byte[] buffer) { return ReceiveCore(buffer, 0, buffer.Length); }

        public int Receive(byte[] buffer, int offset, int size, SocketFlags socketFlags)
        {
            return ReceiveCore(buffer, offset, size);
        }

        private int ReceiveCore(byte[] buffer, int offset, int size)
        {
            int received;
            while ((received = ReceivePoll(_handle, buffer, offset, size)) == WouldBlock) { }
            if (received == SockError) throw new SocketException();
            return received;
        }

        public EndPoint LocalEndPoint
        {
            get { return new IPEndPoint(IPAddress.Loopback, LocalPort(_handle)); }
        }

        public void Close()
        {
            if (_handle >= 0)
            {
                CloseSocket(_handle);
                _handle = -1;
            }
        }

        [Lamella.Runtime.RuntimeProvided] private static int ConnectStart(byte[] addr, int port) { return 0; }
        [Lamella.Runtime.RuntimeProvided] private static int ConnectPoll(int handle) { return 0; }
        [Lamella.Runtime.RuntimeProvided] private static int ListenStart(byte[] addr, int port, int backlog) { return 0; }
        [Lamella.Runtime.RuntimeProvided] private static int AcceptPoll(int handle) { return 0; }
        [Lamella.Runtime.RuntimeProvided] private static int SendPoll(int handle, byte[] buffer, int offset, int count) { return 0; }
        [Lamella.Runtime.RuntimeProvided] private static int ReceivePoll(int handle, byte[] buffer, int offset, int count) { return 0; }
        [Lamella.Runtime.RuntimeProvided] private static int LocalPort(int handle) { return 0; }
        [Lamella.Runtime.RuntimeProvided] private static void CloseSocket(int handle) { }
        [Lamella.Runtime.RuntimeProvided] private static int UdpBind(byte[] addr, int port) { return 0; }
        [Lamella.Runtime.RuntimeProvided] private static int UdpSendTo(int handle, byte[] buffer, int offset, int count, byte[] addr, int port) { return 0; }
        [Lamella.Runtime.RuntimeProvided] private static int UdpReceiveFrom(int handle, byte[] buffer, int offset, int count, byte[] senderAddr, int[] senderMeta) { return 0; }
    }
}
