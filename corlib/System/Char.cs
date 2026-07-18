// Lamella managed corlib (from scratch). -- System.Char
using System.Globalization;
namespace System
{
    public struct Char : IComparable
    {

        public static UnicodeCategory GetUnicodeCategory(char c) { return (UnicodeCategory)CharCategoryData.Category(c); }

        public static bool IsLetter(char c)
        {
            if (c < 0x80) return (c >= 'A' && c <= 'Z') || (c >= 'a' && c <= 'z');
            return (int)GetUnicodeCategory(c) <= (int)UnicodeCategory.OtherLetter;
        }

        public static bool IsDigit(char c)
        {
            if (c < 0x80) return c >= '0' && c <= '9';
            return GetUnicodeCategory(c) == UnicodeCategory.DecimalDigitNumber;
        }

        public static bool IsLetterOrDigit(char c)
        {
            if (c < 0x80) return (c >= 'A' && c <= 'Z') || (c >= 'a' && c <= 'z') || (c >= '0' && c <= '9');
            int cat = (int)GetUnicodeCategory(c);
            return cat <= (int)UnicodeCategory.OtherLetter || cat == (int)UnicodeCategory.DecimalDigitNumber;
        }

        public static bool IsUpper(char c)
        {
            if (c < 0x80) return c >= 'A' && c <= 'Z';
            return GetUnicodeCategory(c) == UnicodeCategory.UppercaseLetter;
        }

        public static bool IsLower(char c)
        {
            if (c < 0x80) return c >= 'a' && c <= 'z';
            return GetUnicodeCategory(c) == UnicodeCategory.LowercaseLetter;
        }

        public static bool IsNumber(char c)
        {
            if (c < 0x80) return c >= '0' && c <= '9';
            int cat = (int)GetUnicodeCategory(c);
            return cat >= (int)UnicodeCategory.DecimalDigitNumber && cat <= (int)UnicodeCategory.OtherNumber;
        }

        public static bool IsWhiteSpace(char c)
        {
            if (c < 0x80) return c == ' ' || (c >= '\t' && c <= '\r');
            if (c == '\u0085') return true;
            int cat = (int)GetUnicodeCategory(c);
            return cat >= (int)UnicodeCategory.SpaceSeparator && cat <= (int)UnicodeCategory.ParagraphSeparator;
        }

        public static bool IsPunctuation(char c)
        {
            int cat = (int)GetUnicodeCategory(c);
            return cat >= (int)UnicodeCategory.ConnectorPunctuation && cat <= (int)UnicodeCategory.OtherPunctuation;
        }

        public static bool IsSymbol(char c)
        {
            int cat = (int)GetUnicodeCategory(c);
            return cat >= (int)UnicodeCategory.MathSymbol && cat <= (int)UnicodeCategory.OtherSymbol;
        }

        public static bool IsSeparator(char c)
        {
            if (c < 0x80) return c == ' ';
            int cat = (int)GetUnicodeCategory(c);
            return cat >= (int)UnicodeCategory.SpaceSeparator && cat <= (int)UnicodeCategory.ParagraphSeparator;
        }

        public static bool IsControl(char c) { return c <= '\u001F' || (c >= '\u007F' && c <= '\u009F'); }

        public static bool IsSurrogate(char c) { return c >= '\uD800' && c <= '\uDFFF'; }

        public static double GetNumericValue(char c) { return CharNumericData.Value(c); }

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
