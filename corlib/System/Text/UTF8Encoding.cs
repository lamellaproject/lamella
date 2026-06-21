// Lamella managed corlib (from scratch). -- System.Text.UTF8Encoding
namespace System.Text
{
    public class UTF8Encoding : Encoding
    {
        public UTF8Encoding() { }

        private static bool IsHighSurrogate(int c) { return c >= 0xD800 && c <= 0xDBFF; }
        private static bool IsLowSurrogate(int c) { return c >= 0xDC00 && c <= 0xDFFF; }

        private static int BytesForScalar(int cp)
        {
            if (cp < 0x80) return 1;
            if (cp < 0x800) return 2;
            if (cp < 0x10000) return 3;
            return 4;
        }

        public override int GetByteCount(string s)
        {
            int count = 0;
            int i = 0;
            int n = s.Length;
            while (i < n)
            {
                int c = s[i];
                if (IsHighSurrogate(c) && i + 1 < n && IsLowSurrogate(s[i + 1]))
                {
                    int cp = 0x10000 + ((c - 0xD800) << 10) + (s[i + 1] - 0xDC00);
                    count = count + BytesForScalar(cp);
                    i = i + 2;
                }
                else
                {
                    count = count + BytesForScalar(c);
                    i = i + 1;
                }
            }
            return count;
        }

        public override byte[] GetBytes(string s)
        {
            byte[] bytes = new byte[GetByteCount(s)];
            int pos = 0;
            int i = 0;
            int n = s.Length;
            while (i < n)
            {
                int cp;
                int c = s[i];
                if (IsHighSurrogate(c) && i + 1 < n && IsLowSurrogate(s[i + 1]))
                {
                    cp = 0x10000 + ((c - 0xD800) << 10) + (s[i + 1] - 0xDC00);
                    i = i + 2;
                }
                else
                {
                    cp = c;
                    i = i + 1;
                }
                if (cp < 0x80)
                {
                    bytes[pos] = (byte)cp;
                    pos = pos + 1;
                }
                else if (cp < 0x800)
                {
                    bytes[pos] = (byte)(0xC0 | (cp >> 6));
                    bytes[pos + 1] = (byte)(0x80 | (cp & 0x3F));
                    pos = pos + 2;
                }
                else if (cp < 0x10000)
                {
                    bytes[pos] = (byte)(0xE0 | (cp >> 12));
                    bytes[pos + 1] = (byte)(0x80 | ((cp >> 6) & 0x3F));
                    bytes[pos + 2] = (byte)(0x80 | (cp & 0x3F));
                    pos = pos + 3;
                }
                else
                {
                    bytes[pos] = (byte)(0xF0 | (cp >> 18));
                    bytes[pos + 1] = (byte)(0x80 | ((cp >> 12) & 0x3F));
                    bytes[pos + 2] = (byte)(0x80 | ((cp >> 6) & 0x3F));
                    bytes[pos + 3] = (byte)(0x80 | (cp & 0x3F));
                    pos = pos + 4;
                }
            }
            return bytes;
        }

        public override string GetString(byte[] bytes)
        {
            StringBuilder result = new StringBuilder();
            int i = 0;
            int n = bytes.Length;
            while (i < n)
            {
                int b0 = bytes[i];
                int cp;
                if (b0 < 0x80)
                {
                    cp = b0;
                    i = i + 1;
                }
                else if (b0 < 0xE0)
                {
                    cp = ((b0 & 0x1F) << 6) | (bytes[i + 1] & 0x3F);
                    i = i + 2;
                }
                else if (b0 < 0xF0)
                {
                    cp = ((b0 & 0x0F) << 12) | ((bytes[i + 1] & 0x3F) << 6) | (bytes[i + 2] & 0x3F);
                    i = i + 3;
                }
                else
                {
                    cp = ((b0 & 0x07) << 18) | ((bytes[i + 1] & 0x3F) << 12)
                        | ((bytes[i + 2] & 0x3F) << 6) | (bytes[i + 3] & 0x3F);
                    i = i + 4;
                }
                if (cp >= 0x10000)
                {
                    int v = cp - 0x10000;
                    char high = (char)(0xD800 + (v >> 10));
                    char low = (char)(0xDC00 + (v & 0x3FF));
                    result.Append(high);
                    result.Append(low);
                }
                else
                {
                    char ch = (char)cp;
                    result.Append(ch);
                }
            }
            return result.ToString();
        }
    }
}
