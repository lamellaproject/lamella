// Lamella managed corlib (from scratch). -- System.Int16
namespace System
{
    public struct Int16 : IComparable
    {
        public const short MaxValue = 32767;
        public const short MinValue = -32768;

        public override string ToString()
        {
            int value = this;
            if (value == 0) return "0";
            bool negative = value < 0;
            int n = negative ? value : -value;
            char[] buffer = new char[8];
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

        public bool Equals(short obj) { return this == obj; }

        public override bool Equals(object obj)
        {
            if (obj is short) return this == (short)obj;
            return false;
        }

        public override int GetHashCode()
        {
            int value = this;
            return value ^ (value << 16);
        }

        public int CompareTo(short value)
        {
            if (this < value) return -1;
            if (this > value) return 1;
            return 0;
        }

        public int CompareTo(object obj)
        {
            if (obj == null) return 1;
            return CompareTo((short)obj);
        }

        public static short Parse(string s)
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
                if (result < (-32768 + digit) / 10) throw new OverflowException("Value was either too large or too small for an Int16.");
                result = result * 10 - digit;
                i = i + 1;
            }
            if (!negative)
            {
                if (result < -32767) throw new OverflowException("Value was either too large or too small for an Int16.");
                return (short)(-result);
            }
            return (short)result;
        }
    }
}
