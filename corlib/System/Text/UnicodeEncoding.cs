// Lamella managed corlib (from scratch). -- System.Text.UnicodeEncoding
namespace System.Text
{
    public class UnicodeEncoding : Encoding
    {
        public UnicodeEncoding() { }

        public override int GetByteCount(string s) { return s.Length * 2; }

        public override byte[] GetBytes(string s)
        {
            byte[] bytes = new byte[s.Length * 2];
            for (int i = 0; i < s.Length; i++)
            {
                int c = s[i];
                bytes[2 * i] = (byte)(c & 0xFF);
                bytes[2 * i + 1] = (byte)((c >> 8) & 0xFF);
            }
            return bytes;
        }

        public override string GetString(byte[] bytes)
        {
            StringBuilder result = new StringBuilder();
            int i = 0;
            while (i + 1 < bytes.Length)
            {
                int lo = bytes[i];
                int hi = bytes[i + 1];
                result.Append((char)(lo | (hi << 8)));
                i = i + 2;
            }
            return result.ToString();
        }
    }
}
