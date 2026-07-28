// Lamella managed corlib (from scratch). -- System.Math
namespace System
{
    public sealed class Math
    {
#if LAMELLA_SURFACE_FLOAT
        public const double PI = 3.14159265358979323846;
        public const double E = 2.7182818284590452354;
#endif

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

#if LAMELLA_SURFACE_FLOAT
        [Lamella.Runtime.RuntimeProvided] public static double Abs(double value) { return 0; }
        [Lamella.Runtime.RuntimeProvided] public static double Max(double a, double b) { return 0; }
        [Lamella.Runtime.RuntimeProvided] public static double Min(double a, double b) { return 0; }
        [Lamella.Runtime.RuntimeProvided] public static int Sign(double value) { return 0; }

        [Lamella.Runtime.RuntimeProvided] public static double Floor(double d) { return 0; }
        [Lamella.Runtime.RuntimeProvided] public static double Ceiling(double a) { return 0; }
        [Lamella.Runtime.RuntimeProvided] public static double Round(double a) { return 0; }
#if LAMELLA_SURFACE_NETFX_2_0
        [Lamella.Runtime.RuntimeProvided] public static double Truncate(double d) { return 0; }
#endif
#endif

#if LAMELLA_SURFACE_MATH_TRANSCENDENTAL
        [Lamella.Runtime.RuntimeProvided] public static double Sqrt(double d) { return 0; }
        [Lamella.Runtime.RuntimeProvided] public static double Pow(double x, double y) { return 0; }
        [Lamella.Runtime.RuntimeProvided] public static double Sin(double a) { return 0; }
        [Lamella.Runtime.RuntimeProvided] public static double Cos(double d) { return 0; }
        [Lamella.Runtime.RuntimeProvided] public static double Tan(double a) { return 0; }
        [Lamella.Runtime.RuntimeProvided] public static double Log(double d) { return 0; }
        [Lamella.Runtime.RuntimeProvided] public static double Log10(double d) { return 0; }
        [Lamella.Runtime.RuntimeProvided] public static double Exp(double d) { return 0; }

        [Lamella.Runtime.RuntimeProvided] public static double Asin(double d) { return 0; }
        [Lamella.Runtime.RuntimeProvided] public static double Acos(double d) { return 0; }
        [Lamella.Runtime.RuntimeProvided] public static double Atan(double d) { return 0; }
        [Lamella.Runtime.RuntimeProvided] public static double Atan2(double y, double x) { return 0; }
        [Lamella.Runtime.RuntimeProvided] public static double Sinh(double value) { return 0; }
        [Lamella.Runtime.RuntimeProvided] public static double Cosh(double value) { return 0; }
        [Lamella.Runtime.RuntimeProvided] public static double Tanh(double value) { return 0; }
        [Lamella.Runtime.RuntimeProvided] public static double IEEERemainder(double x, double y) { return 0; }
        [Lamella.Runtime.RuntimeProvided] public static double Log(double a, double newBase) { return 0; }
#endif
    }
}
