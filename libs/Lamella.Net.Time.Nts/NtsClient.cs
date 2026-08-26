// Lamella.Net.Time.Nts -- the RFC 8915 Network Time Security client (the authenticated add-on to the base Lamella.Net.Time SNTP client).
using System;
using System.Net;
using System.Net.Sockets;

namespace Lamella.Net.Time
{
    public sealed class NtsClient
    {
        private readonly string _keServer;
        private readonly int _kePort;
        private int _timeout;

        public NtsClient(string keServer) : this(keServer, 4460) { }

        public NtsClient(string keServer, int kePort)
        {
            if ((object)keServer == null) throw new ArgumentNullException("keServer");
            _keServer = keServer;
            _kePort = kePort;
            _timeout = 5000;
        }

        public int Timeout
        {
            get { return _timeout; }
            set { _timeout = value < 1 ? 1 : value; }
        }

        public SyncResult SyncOnce()
        {
            int stack = NtsNative.DefaultStack();
            int config = NtsNative.ClientConfigAlpn(stack, 0, null, "ntske/1");
            if (config < 0) return Fail("the TLS backend cannot negotiate NTS-KE (no ALPN / below TLS 1.3)");
            int tls = NtsNative.ClientNew(config, _keServer);
            if (tls < 0) return Fail("could not start the NTS-KE TLS session");
            TcpClient ke = new TcpClient();
            try
            {
                IPAddress[] addresses = Dns.GetHostAddresses(_keServer);
                if (addresses.Length == 0) return Fail("could not resolve '" + _keServer + "'");
                ke.Connect(new IPEndPoint(addresses[0], _kePort));
                ke.Client.ReceiveTimeout = _timeout;
                NetworkStream wire = ke.GetStream();
                byte[] xfer = new byte[16640];

                if (!KeHandshake(tls, wire, xfer)) return Fail("the NTS-KE TLS handshake failed");
                if (NtsNative.AlpnIs(tls, "ntske/1") == 0)
                    return Fail("the server did not select the ntske/1 protocol");

                byte[] request = NtsProtocol.BuildKeRequest();
                if (!SendAll(tls, wire, xfer, request)) return Fail("could not send the NTS-KE request");
                byte[] records = ReadKeRecords(tls, wire, xfer);
                if ((object)records == null) return Fail("no NTS-KE response");

                NtsSession session = EstablishSession(tls, records);
                if ((object)session == null) return Fail("NTS-KE negotiation incomplete");

                return ProtectedSync(session);
            }
            catch (SocketException e)
            {
                return Fail(e.Message);
            }
            finally
            {
                ke.Close();
                NtsNative.CloseTls(tls);
            }
        }

        private SyncResult ProtectedSync(NtsSession session)
        {
            string server = session.NtpServer;
            if ((object)server == null) server = _keServer;
            int port = session.NtpPort;
            if (port <= 0) port = 123;
            IPAddress[] addresses = Dns.GetHostAddresses(server);
            if (addresses.Length == 0) return Fail("could not resolve '" + server + "'");
            IPEndPoint endpoint = new IPEndPoint(addresses[0], port);

            Random random = new Random(unchecked((int)DateTime.UtcNow.Ticks));
            byte[] uniqueId = FillRandom(random, 32);
            byte[] nonce = FillRandom(random, 16);

            long t1 = DateTime.UtcNow.Ticks;
            byte[] header = NtpPacket.BuildClientHeader(t1);
            int placeholders = 8 - session.CookieCount;
            if (placeholders < 0) placeholders = 0;
            byte[] datagram = session.BuildRequest(header, uniqueId, nonce, placeholders);
            if ((object)datagram == null) return Fail("no NTS cookie available for the request");

            UdpClient udp = new UdpClient(0);
            try
            {
                udp.Client.ReceiveTimeout = _timeout;
                udp.Send(datagram, datagram.Length, endpoint);
                IPEndPoint from = null;
                byte[] reply = udp.Receive(ref from);
                long t4 = DateTime.UtcNow.Ticks;
                if (!session.VerifyResponse(reply))
                    return Fail("the NTS response failed authentication");
                return NtpPacket.Evaluate(header, reply, t1, t4, "nts", true);
            }
            finally
            {
                udp.Close();
            }
        }

        private bool KeHandshake(int tls, NetworkStream wire, byte[] xfer)
        {
            for (int step = 0; step < 64; step++)
            {
                int state = NtsNative.Process(tls);
                FlushOutgoing(tls, wire, xfer);
                if (state == 1) return true;
                if (state == 3) return false;
                int received = wire.Read(xfer, 0, xfer.Length);
                if (received <= 0) return false;
                FeedIncoming(tls, xfer, received);
            }
            return false;
        }

        private void FlushOutgoing(int tls, NetworkStream wire, byte[] xfer)
        {
            while (NtsNative.WantsWrite(tls) != 0)
            {
                int produced = NtsNative.WriteTls(tls, xfer, 0, xfer.Length);
                if (produced <= 0) break;
                wire.Write(xfer, 0, produced);
            }
            wire.Flush();
        }

        private static void FeedIncoming(int tls, byte[] xfer, int count)
        {
            int fed = 0;
            while (fed < count)
            {
                int consumed = NtsNative.ReadTls(tls, xfer, fed, count - fed);
                fed += consumed;
                NtsNative.Process(tls);
                if (consumed == 0) break;
            }
        }

        private bool SendAll(int tls, NetworkStream wire, byte[] xfer, byte[] data)
        {
            int written = 0;
            while (written < data.Length)
            {
                int queued = NtsNative.WritePlain(tls, data, written, data.Length - written);
                if (queued <= 0) return false;
                written += queued;
                FlushOutgoing(tls, wire, xfer);
            }
            return true;
        }

        private byte[] ReadKeRecords(int tls, NetworkStream wire, byte[] xfer)
        {
            byte[] accumulated = new byte[4096];
            int total = 0;
            for (int step = 0; step < 64; step++)
            {
                int got = NtsNative.ReadPlain(tls, accumulated, total, accumulated.Length - total);
                if (got > 0)
                {
                    total += got;
                    if (HasEndOfMessage(accumulated, total)) break;
                    if (total == accumulated.Length) break;
                    continue;
                }
                int received = wire.Read(xfer, 0, xfer.Length);
                if (received <= 0) break;
                FeedIncoming(tls, xfer, received);
            }
            if (total <= 0) return null;
            byte[] records = new byte[total];
            for (int i = 0; i < total; i++) records[i] = accumulated[i];
            return records;
        }

        private static bool HasEndOfMessage(byte[] records, int length)
        {
            int offset = 0;
            while (offset + 4 <= length)
            {
                int type = ((records[offset] << 8) | records[offset + 1]) & 0x7FFF;
                int len = (records[offset + 2] << 8) | records[offset + 3];
                if (offset + 4 + len > length) return false;
                if (type == NtsProtocol.RecordEndOfMessage) return true;
                offset += 4 + len;
            }
            return false;
        }

        private static byte[] FillRandom(Random random, int count)
        {
            byte[] bytes = new byte[count];
            for (int i = 0; i < count; i++) bytes[i] = (byte)random.Next(256);
            return bytes;
        }

        private NtsSession EstablishSession(int tls, byte[] records)
        {
            bool okProtocol = false;
            bool okAead = false;
            byte[][] cookies = new byte[8][];
            int cookieCount = 0;
            string ntpServer = null;
            int ntpPort = 0;

            int offset = 0;
            while (offset + 4 <= records.Length)
            {
                int typeField = (records[offset] << 8) | records[offset + 1];
                int type = typeField & 0x7FFF;
                int len = (records[offset + 2] << 8) | records[offset + 3];
                int body = offset + 4;
                if (body + len > records.Length) break;
                if (type == NtsProtocol.RecordNextProtocol && len >= 2)
                {
                    int proto = (records[body] << 8) | records[body + 1];
                    if (proto == NtsProtocol.NextProtocolNtpv4) okProtocol = true;
                }
                else if (type == NtsProtocol.RecordAeadAlgorithm && len >= 2)
                {
                    int aead = (records[body] << 8) | records[body + 1];
                    if (aead == NtsProtocol.AeadAesSivCmac256) okAead = true;
                }
                else if (type == NtsProtocol.RecordNewCookie && cookieCount < 8)
                {
                    byte[] cookie = new byte[len];
                    for (int i = 0; i < len; i++) cookie[i] = records[body + i];
                    cookies[cookieCount] = cookie;
                    cookieCount++;
                }
                else if (type == NtsProtocol.RecordError)
                {
                    return null;
                }
                else if (type == NtsProtocol.RecordNtpv4Server && len > 0)
                {
                    string name = "";
                    for (int i = 0; i < len; i++) name = name + ((char)records[body + i]).ToString();
                    ntpServer = name;
                }
                else if (type == NtsProtocol.RecordNtpv4Port && len >= 2)
                {
                    ntpPort = (records[body] << 8) | records[body + 1];
                }
                else if (type == NtsProtocol.RecordEndOfMessage)
                {
                    break;
                }
                offset = body + len;
            }
            if (!okProtocol || !okAead || cookieCount == 0) return null;

            int c2s = NtsNative.ExporterKey(tls, "EXPORTER-network-time-security", NtsProtocol.ExporterContext(true), 32);
            int s2c = NtsNative.ExporterKey(tls, "EXPORTER-network-time-security", NtsProtocol.ExporterContext(false), 32);
            if (c2s < 0 || s2c < 0) return null;
            return new NtsSession(c2s, s2c, cookies, cookieCount, ntpServer, ntpPort);
        }

        private static SyncResult Fail(string warning)
        {
            return SyncResult.Failed("nts", warning);
        }
    }
}
