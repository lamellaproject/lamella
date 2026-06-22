// Lamella managed corlib (from scratch). -- System.Boolean
namespace System
{
    public struct Boolean : IComparable
    {
        public static readonly string TrueString = "True";
        public static readonly string FalseString = "False";

        public override string ToString() { return this ? TrueString : FalseString; }

        private static bool MatchesIgnoreCase(string s, int at, string text)
        {
            int n = text.Length;
            for (int i = 0; i < n; i++)
            {
                if (Char.ToLower(s[at + i]) != Char.ToLower(text[i])) return false;
            }
            return true;
        }

        public static bool Parse(string value)
        {
            if ((object)value == null) throw new ArgumentNullException("value");
            int start = 0;
            int end = value.Length - 1;
            while (start <= end && Char.IsWhiteSpace(value[start])) start++;
            while (end >= start && Char.IsWhiteSpace(value[end])) end--;
            int len = end - start + 1;
            if (len == 4 && MatchesIgnoreCase(value, start, TrueString)) return true;
            if (len == 5 && MatchesIgnoreCase(value, start, FalseString)) return false;
            throw new FormatException("String was not recognized as a valid Boolean.");
        }

        public static bool TryParse(string value, out bool result)
        {
            result = false;
            if ((object)value == null) return false;
            int start = 0;
            int end = value.Length - 1;
            while (start <= end && Char.IsWhiteSpace(value[start])) start++;
            while (end >= start && Char.IsWhiteSpace(value[end])) end--;
            int len = end - start + 1;
            if (len == 4 && MatchesIgnoreCase(value, start, TrueString)) { result = true; return true; }
            if (len == 5 && MatchesIgnoreCase(value, start, FalseString)) { result = false; return true; }
            return false;
        }

        public bool Equals(bool obj) { return this == obj; }

        public override bool Equals(object obj)
        {
            if (obj is bool) return this == (bool)obj;
            return false;
        }

        public override int GetHashCode() { return this ? 1 : 0; }

        public int CompareTo(bool value)
        {
            if (this == value) return 0;
            return this ? 1 : -1;
        }

        public int CompareTo(object obj)
        {
            if (obj == null) return 1;
            return CompareTo((bool)obj);
        }
    }
}
