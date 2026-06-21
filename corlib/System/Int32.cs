// Lamella managed corlib (from scratch). -- System.Int32
namespace System
{
    public struct Int32 : IComparable
    {
        public int CompareTo(object obj)
        {
            if (obj == null) return 1;
            int other = (int)obj;
            if (this < other) return -1;
            if (this > other) return 1;
            return 0;
        }

        public static int Parse(string s)
        {
            int result = 0;
            int i = 0;
            bool negative = false;
            if (s[0] == '-') { negative = true; i = 1; }
            else if (s[0] == '+') { i = 1; }
            while (i < s.Length)
            {
                result = result * 10 + (s[i] - '0');
                i = i + 1;
            }
            return negative ? -result : result;
        }
    }
}
