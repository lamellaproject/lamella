// Lamella managed corlib (from scratch). -- System.Text.Encoding
namespace System.Text
{
    public abstract class Encoding
    {
        private static UTF8Encoding _utf8;
        private static ASCIIEncoding _ascii;
        private static UnicodeEncoding _unicode;

        static Encoding()
        {
            _utf8 = new UTF8Encoding();
            _ascii = new ASCIIEncoding();
            _unicode = new UnicodeEncoding();
        }

        public static Encoding UTF8 { get { return _utf8; } }

        public static Encoding ASCII { get { return _ascii; } }
        public static Encoding Unicode { get { return _unicode; } }

        public abstract byte[] GetBytes(string s);
        public abstract string GetString(byte[] bytes);
        public abstract int GetByteCount(string s);
    }
}
