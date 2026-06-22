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
    }
}
