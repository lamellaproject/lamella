// Lamella managed corlib (from scratch). -- System.Convert
namespace System
{
    public sealed class Convert
    {
        public static int ToInt32(string value)
        {
            if ((object)value == null) return 0;
            return Int32.Parse(value);
        }

        public static long ToInt64(string value)
        {
            if ((object)value == null) return 0;
            return Int64.Parse(value);
        }

        public static string ToString(int value) { return value.ToString(); }
        public static string ToString(long value) { return value.ToString(); }
        public static string ToString(bool value) { return value ? "True" : "False"; }

        public static bool ToBoolean(string value)
        {
            if ((object)value == null) return false;
            return Boolean.Parse(value);
        }
        public static bool ToBoolean(int value) { return value != 0; }

        public static char ToChar(int value)
        {
            if (value < 0 || value > 65535) throw new OverflowException("Value was either too large or too small for a character.");
            return (char)value;
        }

        public static byte ToByte(int value)
        {
            if (value < 0 || value > 255) throw new OverflowException("Value was either too large or too small for an unsigned byte.");
            return (byte)value;
        }

        public static byte[] FromBase64String(string s)
        {
            if ((object)s == null) throw new ArgumentNullException("s");
            byte[] buffer = new byte[s.Length];
            int count = 0;
            int accumulator = 0;
            int bits = 0;
            for (int i = 0; i < s.Length; i++)
            {
                char c = s[i];
                if (c == ' ' || c == '\t' || c == '\r' || c == '\n') continue;
                if (c == '=') break;
                int value = Base64Value(c);
                if (value < 0) throw new FormatException("The input is not a valid base-64 string.");
                accumulator = (accumulator << 6) | value;
                bits += 6;
                if (bits >= 8)
                {
                    bits -= 8;
                    buffer[count++] = (byte)((accumulator >> bits) & 0xFF);
                }
            }
            if (count == buffer.Length) return buffer;
            byte[] result = new byte[count];
            Array.Copy(buffer, result, count);
            return result;
        }

        private static int Base64Value(char c)
        {
            if (c >= 'A' && c <= 'Z') return c - 'A';
            if (c >= 'a' && c <= 'z') return c - 'a' + 26;
            if (c >= '0' && c <= '9') return c - '0' + 52;
            if (c == '+') return 62;
            if (c == '/') return 63;
            return -1;
        }
    }
}
