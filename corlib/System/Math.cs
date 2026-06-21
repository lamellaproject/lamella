// Lamella managed corlib (from scratch). -- System.Math
namespace System
{
    public sealed class Math
    {
        public static int Max(int a, int b) { return a >= b ? a : b; }
        public static int Min(int a, int b) { return a <= b ? a : b; }

        public static int Abs(int value)
        {
            if (value == Int32.MinValue) throw new OverflowException("Negating the minimum value of a twos complement number is invalid.");
            return value < 0 ? -value : value;
        }

        public static int Sign(int value) { return value > 0 ? 1 : (value < 0 ? -1 : 0); }

        public static long Max(long a, long b) { return a >= b ? a : b; }
        public static long Min(long a, long b) { return a <= b ? a : b; }

        public static long Abs(long value)
        {
            if (value == Int64.MinValue) throw new OverflowException("Negating the minimum value of a twos complement number is invalid.");
            return value < 0 ? -value : value;
        }

        public static int Sign(long value) { return value > 0 ? 1 : (value < 0 ? -1 : 0); }
    }
}
