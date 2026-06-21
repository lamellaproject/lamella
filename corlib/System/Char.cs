// Lamella managed corlib (from scratch). -- System.Char
namespace System
{
    public struct Char
    {
        public static bool IsDigit(char c) { return c >= '0' && c <= '9'; }
        public static bool IsLetter(char c) { return (c >= 'A' && c <= 'Z') || (c >= 'a' && c <= 'z'); }
        public static bool IsWhiteSpace(char c) { return c == ' ' || c == '\t' || c == '\n' || c == '\r'; }

        public static char ToUpper(char c) { if (c >= 'a' && c <= 'z') return (char)(c - 32); return c; }
        public static char ToLower(char c) { if (c >= 'A' && c <= 'Z') return (char)(c + 32); return c; }

        [Lamella.Runtime.RuntimeProvided] public override string ToString() { return null; }
    }
}
