// Lamella managed corlib (from scratch). -- System.Int64
namespace System
{
    public struct Int64 : IComparable
    {
        public const long MaxValue = 9223372036854775807;
        public const long MinValue = -9223372036854775808;

        public override bool Equals(object obj)
        {
            if (obj is long) return this == (long)obj;
            return false;
        }

        public bool Equals(long obj) { return this == obj; }

        public override int GetHashCode()
        {
            long value = this;
            return (int)value ^ (int)(value >> 32);
        }

        public int CompareTo(long value)
        {
            if (this < value) return -1;
            if (this > value) return 1;
            return 0;
        }

        public int CompareTo(object obj)
        {
            if (obj == null) return 1;
            return CompareTo((long)obj);
        }

        public override string ToString()
        {
            long value = this;
            if (value == 0) return "0";
            bool negative = value < 0;
            long n = negative ? value : -value;
            char[] buffer = new char[24];
            int pos = buffer.Length;
            while (n != 0)
            {
                int digit = (int)(-(n % 10));
                pos = pos - 1;
                buffer[pos] = (char)('0' + digit);
                n = n / 10;
            }
            System.Text.StringBuilder result = new System.Text.StringBuilder();
            if (negative) result.Append('-');
            for (int i = pos; i < buffer.Length; i++) result.Append(buffer[i]);
            return result.ToString();
        }

        public static long Parse(string s)
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
            long result = 0;
            while (i < end)
            {
                char c = s[i];
                if (c < '0' || c > '9') throw new FormatException("Input string was not in a correct format.");
                int digit = c - '0';
                if (result < (MinValue + digit) / 10) throw new OverflowException("Value was either too large or too small for an Int64.");
                result = result * 10 - digit;
                i = i + 1;
            }
            if (!negative)
            {
                if (result == MinValue) throw new OverflowException("Value was either too large or too small for an Int64.");
                return -result;
            }
            return result;
        }

        public static bool TryParse(string s, out long result)
        {
            result = 0;
            if (s == null || s.Length == 0) return false;
            int end = s.Length;
            while (end > 0 && Char.IsWhiteSpace(s[end - 1])) end = end - 1;
            int i = 0;
            while (i < end && Char.IsWhiteSpace(s[i])) i = i + 1;
            bool negative = false;
            if (i < end && s[i] == '-') { negative = true; i = i + 1; }
            else if (i < end && s[i] == '+') { i = i + 1; }
            if (i >= end) return false;
            long value = 0;
            while (i < end)
            {
                char c = s[i];
                if (c < '0' || c > '9') return false;
                int digit = c - '0';
                if (value < (MinValue + digit) / 10) return false;
                value = value * 10 - digit;
                i = i + 1;
            }
            if (!negative)
            {
                if (value == MinValue) return false;
                result = -value;
                return true;
            }
            result = value;
            return true;
        }
    }
}
