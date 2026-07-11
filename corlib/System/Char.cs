// Lamella managed corlib (from scratch). -- System.Char
namespace System
{
    public struct Char : IComparable
    {
        public static bool IsDigit(char c) { return c >= '0' && c <= '9'; }
        public static bool IsLetter(char c) { return (c >= 'A' && c <= 'Z') || (c >= 'a' && c <= 'z'); }
        public static bool IsLetterOrDigit(char c) { return IsLetter(c) || IsDigit(c); }
        public static bool IsWhiteSpace(char c) { return c == ' ' || c == '\t' || c == '\n' || c == '\r'; }
        public static bool IsUpper(char c) { return c >= 'A' && c <= 'Z'; }
        public static bool IsLower(char c) { return c >= 'a' && c <= 'z'; }

        public static char ToUpper(char c) { return CaseMapping.ToUpper(c); }
        public static char ToLower(char c) { return CaseMapping.ToLower(c); }
#if LAMELLA_SURFACE_STRING_COMPARISON
        public static char ToUpperInvariant(char c) { return CaseMapping.ToUpper(c); }
        public static char ToLowerInvariant(char c) { return CaseMapping.ToLower(c); }
#endif

        public bool Equals(char obj) { return this == obj; }

        public override bool Equals(object obj)
        {
            if (obj is char) return this == (char)obj;
            return false;
        }

        public override int GetHashCode() { int value = this; return value | (value << 16); }

        public int CompareTo(char value)
        {
            if (this < value) return -1;
            if (this > value) return 1;
            return 0;
        }

        public int CompareTo(object obj)
        {
            if (obj == null) return 1;
            if (!(obj is char)) throw new ArgumentException("Object must be of type Char.");
            return CompareTo((char)obj);
        }

        [Lamella.Runtime.RuntimeProvided] public override string ToString() { return null; }
    }
}
