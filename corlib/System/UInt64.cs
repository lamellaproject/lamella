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
    }
}
