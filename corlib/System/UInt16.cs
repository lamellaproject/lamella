// Lamella managed corlib (from scratch). -- System.UInt16
namespace System
{
    public struct UInt16 : IComparable
    {
        public const ushort MaxValue = 65535;
        public const ushort MinValue = 0;

        public override string ToString()
        {
            int value = this;
            if (value == 0) return "0";
            char[] buffer = new char[8];
            int pos = buffer.Length;
            while (value != 0)
            {
                int digit = value % 10;
                pos = pos - 1;
                buffer[pos] = (char)('0' + digit);
                value = value / 10;
            }
            System.Text.StringBuilder result = new System.Text.StringBuilder();
            for (int i = pos; i < buffer.Length; i++) result.Append(buffer[i]);
            return result.ToString();
        }

        public bool Equals(ushort obj) { return this == obj; }

        public override bool Equals(object obj)
        {
            if (obj is ushort) return this == (ushort)obj;
            return false;
        }

        public override int GetHashCode() { return this; }

        public int CompareTo(ushort value)
        {
            if (this < value) return -1;
            if (this > value) return 1;
            return 0;
        }

        public int CompareTo(object obj)
        {
            if (obj == null) return 1;
            return CompareTo((ushort)obj);
        }

        public static ushort Parse(string s)
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
                long digit = (long)(c - '0');
                if (result > (65535L - digit) / 10) throw new OverflowException("Value was either too large or too small for a UInt16.");
                result = result * 10 + digit;
                i = i + 1;
            }
            if (negative && result != 0) throw new OverflowException("Value was either too large or too small for a UInt16.");
            return (ushort)result;
        }
    }
}
