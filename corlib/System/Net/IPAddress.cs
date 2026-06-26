// Lamella managed corlib (from scratch). -- System.Net.IPAddress
#if LAMELLA_SURFACE_NET
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

        public static IPAddress Parse(string ipString)
        {
            if ((object)ipString == null) throw new ArgumentNullException("ipString");
            IPAddress address;
            if (!TryParse(ipString, out address))
                throw new FormatException("An invalid IP address was specified.");
            return address;
        }

        public static bool TryParse(string ipString, out IPAddress address)
        {
            address = null;
            if ((object)ipString == null) return false;
            for (int i = 0; i < ipString.Length; i++)
            {
                if (ipString[i] == ':') return TryParseV6(ipString, out address);
            }
            return TryParseV4(ipString, out address);
        }

        private static bool TryParseV4(string ipString, out IPAddress address)
        {
            address = null;
            byte[] octets = new byte[4];
            int part = 0;
            int value = 0;
            bool hasDigit = false;
            for (int i = 0; i < ipString.Length; i++)
            {
                char c = ipString[i];
                if (c == '.')
                {
                    if (!hasDigit || part >= 3) return false;
                    octets[part] = (byte)value;
                    part++;
                    value = 0;
                    hasDigit = false;
                }
                else if (c >= '0' && c <= '9')
                {
                    value = value * 10 + (c - '0');
                    if (value > 255) return false;
                    hasDigit = true;
                }
                else
                {
                    return false;
                }
            }
            if (!hasDigit || part != 3) return false;
            octets[3] = (byte)value;
            address = new IPAddress(octets);
            return true;
        }

        private static bool TryParseV6(string s, out IPAddress address)
        {
            address = null;
            byte[] bytes = new byte[16];
            bool hasDot = false;
            for (int i = 0; i < s.Length; i++) { if (s[i] == '.') { hasDot = true; break; } }
            if (hasDot)
            {
                int colon = -1;
                for (int i = s.Length - 1; i >= 0; i--) { if (s[i] == ':') { colon = i; break; } }
                if (colon < 0) return false;
                IPAddress v4;
                if (!TryParseV4(s.Substring(colon + 1), out v4)) return false;
                byte[] v4b = v4.GetAddressBytes();
                int g1 = (v4b[0] << 8) | v4b[1];
                int g2 = (v4b[2] << 8) | v4b[3];
                s = s.Substring(0, colon + 1) + ToHex4(g1) + ":" + ToHex4(g2);
            }
            int dc = -1;
            for (int i = 0; i + 1 < s.Length; i++)
            {
                if (s[i] == ':' && s[i + 1] == ':') { dc = i; break; }
            }
            string head;
            string tail;
            if (dc >= 0)
            {
                head = s.Substring(0, dc);
                tail = s.Substring(dc + 2);
                for (int i = 0; i + 1 < tail.Length; i++)
                {
                    if (tail[i] == ':' && tail[i + 1] == ':') return false;
                }
            }
            else
            {
                head = s;
                tail = null;
            }

            int[] left = new int[8];
            int leftCount;
            if (!ParseHexGroups(head, left, out leftCount)) return false;
            int[] right = new int[8];
            int rightCount = 0;
            if ((object)tail != null && !ParseHexGroups(tail, right, out rightCount)) return false;

            if (dc < 0)
            {
                if (leftCount != 8) return false;
                for (int i = 0; i < 8; i++)
                {
                    bytes[i * 2] = (byte)(left[i] >> 8);
                    bytes[i * 2 + 1] = (byte)(left[i] & 0xFF);
                }
            }
            else
            {
                if (leftCount + rightCount >= 8) return false;
                for (int i = 0; i < leftCount; i++)
                {
                    bytes[i * 2] = (byte)(left[i] >> 8);
                    bytes[i * 2 + 1] = (byte)(left[i] & 0xFF);
                }
                for (int i = 0; i < rightCount; i++)
                {
                    int pos = 8 - rightCount + i;
                    bytes[pos * 2] = (byte)(right[i] >> 8);
                    bytes[pos * 2 + 1] = (byte)(right[i] & 0xFF);
                }
            }
            address = new IPAddress(bytes);
            return true;
        }

        private static bool ParseHexGroups(string s, int[] groups, out int count)
        {
            count = 0;
            if (s.Length == 0) return true;
            int value = 0;
            int digits = 0;
            for (int i = 0; i < s.Length; i++)
            {
                char c = s[i];
                if (c == ':')
                {
                    if (digits == 0 || count >= 8) return false;
                    groups[count] = value;
                    count++;
                    value = 0;
                    digits = 0;
                }
                else
                {
                    int d = HexDigit(c);
                    if (d < 0) return false;
                    value = value * 16 + d;
                    digits++;
                    if (digits > 4) return false;
                }
            }
            if (digits == 0 || count >= 8) return false;
            groups[count] = value;
            count++;
            return true;
        }

        private static int HexDigit(char c)
        {
            if (c >= '0' && c <= '9') return c - '0';
            if (c >= 'a' && c <= 'f') return c - 'a' + 10;
            if (c >= 'A' && c <= 'F') return c - 'A' + 10;
            return -1;
        }

        private static string ToHex4(int value)
        {
            string digits = "0123456789abcdef";
            return digits.Substring((value >> 12) & 0xF, 1)
                + digits.Substring((value >> 8) & 0xF, 1)
                + digits.Substring((value >> 4) & 0xF, 1)
                + digits.Substring(value & 0xF, 1);
        }

        public override string ToString()
        {
            if (_bytes.Length == 4)
            {
                return _bytes[0].ToString() + "." + _bytes[1].ToString() + "."
                    + _bytes[2].ToString() + "." + _bytes[3].ToString();
            }
            return ToStringV6();
        }

        private string ToStringV6()
        {
            bool v4mapped = true;
            for (int i = 0; i < 10; i++) { if (_bytes[i] != 0) { v4mapped = false; break; } }
            if (v4mapped && _bytes[10] == 0xFF && _bytes[11] == 0xFF)
            {
                return "::ffff:" + _bytes[12].ToString() + "." + _bytes[13].ToString() + "."
                    + _bytes[14].ToString() + "." + _bytes[15].ToString();
            }

            int[] groups = new int[8];
            for (int i = 0; i < 8; i++) groups[i] = (_bytes[i * 2] << 8) | _bytes[i * 2 + 1];

            int bestStart = -1;
            int bestLen = 0;
            int curStart = -1;
            int curLen = 0;
            for (int i = 0; i < 8; i++)
            {
                if (groups[i] == 0)
                {
                    if (curStart < 0) { curStart = i; curLen = 1; } else { curLen++; }
                    if (curLen > bestLen) { bestLen = curLen; bestStart = curStart; }
                }
                else { curStart = -1; curLen = 0; }
            }
            if (bestLen < 2) bestStart = -1;

            string result = "";
            bool firstPart = true;
            int idx = 0;
            while (idx < 8)
            {
                if (bestStart >= 0 && idx == bestStart)
                {
                    if (!firstPart) result = result + ":";
                    result = result + "";
                    firstPart = false;
                    idx += bestLen;
                }
                else
                {
                    if (!firstPart) result = result + ":";
                    result = result + GroupHex(groups[idx]);
                    firstPart = false;
                    idx++;
                }
            }
            if (bestStart == 0) result = ":" + result;
            if (bestStart >= 0 && bestStart + bestLen == 8) result = result + ":";
            return result;
        }

        private static string GroupHex(int value)
        {
            if (value == 0) return "0";
            string digits = "0123456789abcdef";
            string result = "";
            while (value > 0)
            {
                result = digits.Substring(value & 0xF, 1) + result;
                value = value >> 4;
            }
            return result;
        }
    }
}
#endif
