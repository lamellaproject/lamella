// Lamella managed corlib (from scratch). -- System.Math
namespace System
{
    public sealed class Math
    {
        public static int Max(int a, int b) { return a >= b ? a : b; }
        public static int Min(int a, int b) { return a <= b ? a : b; }
        public static int Abs(int value) { return value < 0 ? -value : value; }
        public static int Sign(int value) { return value > 0 ? 1 : (value < 0 ? -1 : 0); }
        public static long Max(long a, long b) { return a >= b ? a : b; }
        public static long Min(long a, long b) { return a <= b ? a : b; }
        public static long Abs(long value) { return value < 0 ? -value : value; }
    }
}
