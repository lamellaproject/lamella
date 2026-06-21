// Lamella managed corlib (from scratch). -- System.Text.Encoding
namespace System.Text
{
    public abstract class Encoding
    {
        private static UTF8Encoding _utf8;

        static Encoding()
        {
            _utf8 = new UTF8Encoding();
        }

        public static Encoding UTF8 { get { return _utf8; } }

        public abstract byte[] GetBytes(string s);
        public abstract string GetString(byte[] bytes);
        public abstract int GetByteCount(string s);
    }
}
