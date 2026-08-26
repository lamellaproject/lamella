// Lamella managed corlib (from scratch). -- System.UInt64
namespace System
{
    public struct UInt64 : IComparable
    {
        public const ulong MaxValue = 18446744073709551615;
        public const ulong MinValue = 0;

        public override string ToString()
        {
            ulong value = this;
            if (value == 0) return "0";
            char[] buffer = new char[24];
            int pos = buffer.Length;
            while (value != 0)
            {
                int digit = (int)(value % 10);
                pos = pos - 1;
                buffer[pos] = (char)('0' + digit);
                value = value / 10;
            }
            System.Text.StringBuilder result = new System.Text.StringBuilder();
            for (int i = pos; i < buffer.Length; i++) result.Append(buffer[i]);
            return result.ToString();
        }

        public bool Equals(ulong obj) { return this == obj; }

        public override bool Equals(object obj)
        {
            if (obj is ulong) return this == (ulong)obj;
            return false;
        }

        public override int GetHashCode()
        {
            ulong value = this;
            return (int)value ^ (int)(value >> 32);
        }

        public int CompareTo(ulong value)
        {
            if (this < value) return -1;
            if (this > value) return 1;
            return 0;
        }

        public int CompareTo(object obj)
        {
            if (obj == null) return 1;
            return CompareTo((ulong)obj);
        }

        public static ulong Parse(string s)
        {
            if ((object)s == null) throw new ArgumentNullException("s");
            int end = s.Length;
            while (end > 0 && Char.IsWhiteSpace(s[end - 1])) end = end - 1;
            int i = 0;
            while (i < end && Char.IsWhiteSpace(s[i])) i = i + 1;
            bool negative = false;
            if (i < end && s[i] == '-') { negative = true; i = i + 1; }
            else if (i < end && s[i] == '+') { i = i + 1; }
            if (i >= end) throw new FormatException("Input string was not in a correct format.");
            ulong result = 0;
            while (i < end)
            {
                char c = s[i];
                if (c < '0' || c > '9') throw new FormatException("Input string was not in a correct format.");
                ulong digit = (ulong)(c - '0');
                if (result > (18446744073709551615UL - digit) / 10) throw new OverflowException("Value was either too large or too small for a UInt64.");
                result = result * 10 + digit;
                i = i + 1;
            }
            if (negative && result != 0) throw new OverflowException("Value was either too large or too small for a UInt64.");
            return (ulong)result;
        }
    }
}
