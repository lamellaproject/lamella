// Lamella managed corlib (from scratch). -- System.Net.IPAddress
namespace System.Net
{
    public class IPAddress
    {
        private byte[] _bytes;

        public IPAddress(byte[] address) { _bytes = address; }

        private static byte[] V4(byte a, byte b, byte c, byte d)
        {
            byte[] octets = new byte[4];
            octets[0] = a;
            octets[1] = b;
            octets[2] = c;
            octets[3] = d;
            return octets;
        }

        private static byte[] V6Loopback()
        {
            byte[] octets = new byte[16];
            octets[15] = 1;
            return octets;
        }

        public static readonly IPAddress Loopback = new IPAddress(V4(127, 0, 0, 1));
        public static readonly IPAddress Any = new IPAddress(V4(0, 0, 0, 0));
        public static readonly IPAddress IPv6Loopback = new IPAddress(V6Loopback());
        public static readonly IPAddress IPv6Any = new IPAddress(new byte[16]);

        public byte[] GetAddressBytes() { return _bytes; }

        public override string ToString()
        {
            if (_bytes.Length == 4)
            {
                return _bytes[0].ToString() + "." + _bytes[1].ToString() + "."
                    + _bytes[2].ToString() + "." + _bytes[3].ToString();
            }
            return "[ipv6]";
        }
    }
}
