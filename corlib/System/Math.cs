// Lamella managed corlib (from scratch). -- System.Math
namespace System
{
    public sealed class Math
    {
        private Math() { }

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

        public static sbyte Max(sbyte val1, sbyte val2) { return val1 >= val2 ? val1 : val2; }
        public static sbyte Min(sbyte val1, sbyte val2) { return val1 <= val2 ? val1 : val2; }
        public static byte Max(byte val1, byte val2) { return val1 >= val2 ? val1 : val2; }
        public static byte Min(byte val1, byte val2) { return val1 <= val2 ? val1 : val2; }
        public static short Max(short val1, short val2) { return val1 >= val2 ? val1 : val2; }
        public static short Min(short val1, short val2) { return val1 <= val2 ? val1 : val2; }
        public static ushort Max(ushort val1, ushort val2) { return val1 >= val2 ? val1 : val2; }
        public static ushort Min(ushort val1, ushort val2) { return val1 <= val2 ? val1 : val2; }
        public static uint Max(uint val1, uint val2) { return val1 >= val2 ? val1 : val2; }
        public static uint Min(uint val1, uint val2) { return val1 <= val2 ? val1 : val2; }
        public static ulong Max(ulong val1, ulong val2) { return val1 >= val2 ? val1 : val2; }
        public static ulong Min(ulong val1, ulong val2) { return val1 <= val2 ? val1 : val2; }

        public static sbyte Abs(sbyte value)
        {
            if (value == SByte.MinValue) throw new OverflowException("Negating the minimum value of a twos complement number is invalid.");
            return value < 0 ? (sbyte)-value : value;
        }

        public static short Abs(short value)
        {
            if (value == Int16.MinValue) throw new OverflowException("Negating the minimum value of a twos complement number is invalid.");
            return value < 0 ? (short)-value : value;
        }

        public static int Sign(sbyte value) { return value > 0 ? 1 : (value < 0 ? -1 : 0); }
        public static int Sign(short value) { return value > 0 ? 1 : (value < 0 ? -1 : 0); }

        public static long BigMul(int a, int b) { return (long)a * (long)b; }

        public static int DivRem(int a, int b, out int result)
        {
            result = a % b;
            return a / b;
        }

        public static long DivRem(long a, long b, out long result)
        {
            result = a % b;
            return a / b;
        }

#if LAMELLA_SURFACE_FLOAT
        [Lamella.Runtime.RuntimeProvided] public static double Abs(double value) { return 0; }
        [Lamella.Runtime.RuntimeProvided] public static double Max(double a, double b) { return 0; }
        [Lamella.Runtime.RuntimeProvided] public static double Min(double a, double b) { return 0; }
        [Lamella.Runtime.RuntimeProvided] public static int Sign(double value) { return 0; }

        [Lamella.Runtime.RuntimeProvided] public static double Floor(double d) { return 0; }
        [Lamella.Runtime.RuntimeProvided] public static double Ceiling(double a) { return 0; }
        [Lamella.Runtime.RuntimeProvided] public static double Round(double a) { return 0; }

        public static float Abs(float value) { return (float)Abs((double)value); }
        public static float Max(float val1, float val2) { return (float)Max((double)val1, (double)val2); }
        public static float Min(float val1, float val2) { return (float)Min((double)val1, (double)val2); }
        public static int Sign(float value) { return Sign((double)value); }

        private const double RoundLimit = 1e16;

        public static double Round(double value, int digits)
        {
            if (digits < 0 || digits > 15) throw new ArgumentOutOfRangeException("Rounding digits must be between 0 and 15, inclusive.");
            if (Abs(value) >= RoundLimit) return value;
            double power = Power10(digits);
            return Round(value * power) / power;
        }

        private static double Power10(int digits)
        {
            switch (digits)
            {
                case 0: return 1e0;
                case 1: return 1e1;
                case 2: return 1e2;
                case 3: return 1e3;
                case 4: return 1e4;
                case 5: return 1e5;
                case 6: return 1e6;
                case 7: return 1e7;
                case 8: return 1e8;
                case 9: return 1e9;
                case 10: return 1e10;
                case 11: return 1e11;
                case 12: return 1e12;
                case 13: return 1e13;
                case 14: return 1e14;
                default: return 1e15;
            }
        }
#if LAMELLA_SURFACE_NETFX_2_0
        [Lamella.Runtime.RuntimeProvided] public static double Truncate(double d) { return 0; }
#endif
#endif

#if LAMELLA_SURFACE_DECIMAL
        public static Decimal Abs(Decimal value) { return value < Decimal.Zero ? Decimal.Negate(value) : value; }

        public static int Sign(Decimal value)
        {
            int order = Decimal.Compare(value, Decimal.Zero);
            return order > 0 ? 1 : (order < 0 ? -1 : 0);
        }

        public static Decimal Max(Decimal val1, Decimal val2) { return val1 >= val2 ? val1 : val2; }
        public static Decimal Min(Decimal val1, Decimal val2) { return val1 <= val2 ? val1 : val2; }

        public static Decimal Round(Decimal d) { return Decimal.Round(d, 0); }
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
