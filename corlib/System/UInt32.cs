// Lamella managed corlib (from scratch). -- System.UInt32
namespace System
{
    public struct UInt32 : IComparable
    {
        public const uint MaxValue = 4294967295;
        public const uint MinValue = 0;

        public override string ToString()
        {
            long value = this;
            if (value == 0) return "0";
            char[] buffer = new char[16];
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

        public bool Equals(uint obj) { return this == obj; }

        public override bool Equals(object obj)
        {
            if (obj is uint) return this == (uint)obj;
            return false;
        }

        public override int GetHashCode() { return (int)this; }

        public int CompareTo(uint value)
        {
            if (this < value) return -1;
            if (this > value) return 1;
            return 0;
        }

        public int CompareTo(object obj)
        {
            if (obj == null) return 1;
            return CompareTo((uint)obj);
        }

        public static uint Parse(string s)
        {
            if ((object)s == null) throw new ArgumentNullException("s");
            int end = s.Length;
            while (end > 0 && NumberText.IsPad(s[end - 1])) end = end - 1;
            int i = 0;
            while (i < end && NumberText.IsPad(s[i])) i = i + 1;
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
                if (result > (4294967295L - digit) / 10) throw new OverflowException("Value was either too large or too small for a UInt32.");
                result = result * 10 + digit;
                i = i + 1;
            }
            if (negative && result != 0) throw new OverflowException("Value was either too large or too small for a UInt32.");
            return (uint)result;
        }
    }
}
