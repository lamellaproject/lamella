// Lamella managed corlib (from scratch). -- System.Int32
namespace System
{
    public struct Int32 : IComparable, IFormattable
    {
        public const int MaxValue = 2147483647;
        public const int MinValue = -2147483648;

        public override bool Equals(object obj)
        {
            if (obj is int) return this == (int)obj;
            return false;
        }

        public bool Equals(int obj) { return this == obj; }

        public override int GetHashCode() { return this; }

        public int CompareTo(int value)
        {
            if (this < value) return -1;
            if (this > value) return 1;
            return 0;
        }

        public int CompareTo(object obj)
        {
            if (obj == null) return 1;
            return CompareTo((int)obj);
        }

        public override string ToString()
        {
            int value = this;
            if (value == 0) return "0";
            bool negative = value < 0;
            int n = negative ? value : -value;
            char[] buffer = new char[16];
            int pos = buffer.Length;
            while (n != 0)
            {
                int digit = -(n % 10);
                pos = pos - 1;
                buffer[pos] = (char)('0' + digit);
                n = n / 10;
            }
            System.Text.StringBuilder result = new System.Text.StringBuilder();
            if (negative) result.Append('-');
            for (int i = pos; i < buffer.Length; i++) result.Append(buffer[i]);
            return result.ToString();
        }

        public string ToString(string format)
        {
            return NumberFormatter.Format((long)this, 8, format);
        }

        public string ToString(string format, IFormatProvider formatProvider)
        {
            return NumberFormatter.Format((long)this, 8, format);
        }

        public static int Parse(string s)
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
            int result = 0;
            while (i < end)
            {
                char c = s[i];
                if (c < '0' || c > '9') throw new FormatException("Input string was not in a correct format.");
                int digit = c - '0';
                if (result < (MinValue + digit) / 10) throw new OverflowException("Value was either too large or too small for an Int32.");
                result = result * 10 - digit;
                i = i + 1;
            }
            if (!negative)
            {
                if (result == MinValue) throw new OverflowException("Value was either too large or too small for an Int32.");
                return -result;
            }
            return result;
        }

        public static bool TryParse(string s, out int result)
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
            int value = 0;
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
