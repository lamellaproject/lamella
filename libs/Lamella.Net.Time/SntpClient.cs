// Lamella.Net.Time -- the SNTPv4 (RFC 4330) unicast client, the assembly's base sync protocol.
using System;
using System.Net;
using System.Net.Sockets;

namespace Lamella.Net.Time
{
    public class SntpClient
    {
        private readonly string _server;
        private readonly int _port;
        private int _timeout;

        public SntpClient(string server) : this(server, 123) { }

        public SntpClient(string server, int port)
        {
            if ((object)server == null) throw new ArgumentNullException("server");
            _server = server;
            _port = port;
            _timeout = 3000;
        }

        public int Timeout
        {
            get { return _timeout; }
            set { _timeout = value < 1 ? 1 : value; }
        }

        public SyncResult SyncOnce()
        {
            long t1 = DateTime.UtcNow.Ticks;
            byte[] request = NtpPacket.BuildClientHeader(t1);

            try
            {
                IPAddress[] addresses = Dns.GetHostAddresses(_server);
                if (addresses.Length == 0) return Fail("could not resolve '" + _server + "'");
                IPEndPoint server = new IPEndPoint(addresses[0], _port);

                UdpClient udp = new UdpClient(0);
                try
                {
                    udp.Client.ReceiveTimeout = _timeout;
                    udp.Send(request, 48, server);
                    IPEndPoint from = null;
                    byte[] reply = udp.Receive(ref from);
                    long t4 = DateTime.UtcNow.Ticks;
                    if (from.Port != _port || !SameAddress(from.Address, server.Address))
                        return Fail("reply from an unexpected sender");
                    return NtpPacket.Evaluate(request, reply, t1, t4, "sntp", false);
                }
                finally
                {
                    udp.Close();
                }
            }
            catch (SocketException e)
            {
                return Fail(e.Message);
            }
        }

        private static bool SameAddress(IPAddress a, IPAddress b)
        {
            byte[] left = a.GetAddressBytes();
            byte[] right = b.GetAddressBytes();
            if (left.Length != right.Length) return false;
            for (int i = 0; i < left.Length; i++)
            {
                if (left[i] != right[i]) return false;
            }
            return true;
        }

        private static SyncResult Fail(string warning)
        {
            return new SyncResult(
                false, "sntp", false, 0,
                new TimeSpan(0), new TimeSpan(0), new DateTime(0), warning);
        }
    }
}
