// Lamella managed corlib (from scratch). -- System.Text.ASCIIEncoding
namespace System.Text
{
    public class ASCIIEncoding : Encoding
    {
        public ASCIIEncoding() { }

        public override int GetByteCount(string s) { return s.Length; }

        public override byte[] GetBytes(string s)
        {
            byte[] bytes = new byte[s.Length];
            for (int i = 0; i < s.Length; i++)
            {
                int c = s[i];
                bytes[i] = (byte)(c <= 0x7F ? c : '?');
            }
            return bytes;
        }

        public override string GetString(byte[] bytes)
        {
            StringBuilder result = new StringBuilder();
            for (int i = 0; i < bytes.Length; i++)
            {
                int b = bytes[i];
                result.Append(b <= 0x7F ? (char)b : '?');
            }
            return result.ToString();
        }
    }
}
