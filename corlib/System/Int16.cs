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
    }
}
