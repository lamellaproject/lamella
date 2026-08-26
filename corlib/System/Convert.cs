// Lamella managed corlib (from scratch). -- System.Convert
namespace System
{
    public sealed class Convert
    {
        private Convert() { }

        public static int ToInt32(string value)
        {
            if ((object)value == null) return 0;
            return Int32.Parse(value);
        }

        public static long ToInt64(string value)
        {
            if ((object)value == null) return 0;
            return Int64.Parse(value);
        }

        public static string ToString(int value) { return value.ToString(); }
        public static string ToString(long value) { return value.ToString(); }
        public static string ToString(bool value) { return value ? "True" : "False"; }

        public static bool ToBoolean(string value)
        {
            if ((object)value == null) return false;
            return Boolean.Parse(value);
        }
        public static bool ToBoolean(int value) { return value != 0; }

        public static char ToChar(int value)
        {
            if (value < 0 || value > 65535) throw new OverflowException("Value was either too large or too small for a character.");
            return (char)value;
        }

        public static byte ToByte(int value)
        {
            if (value < 0 || value > 255) throw new OverflowException("Value was either too large or too small for an unsigned byte.");
            return (byte)value;
        }

        public static byte[] FromBase64String(string s)
        {
            if ((object)s == null) throw new ArgumentNullException("s");
            byte[] buffer = new byte[s.Length];
            int count = 0;
            int accumulator = 0;
            int bits = 0;
            for (int i = 0; i < s.Length; i++)
            {
                char c = s[i];
                if (c == ' ' || c == '\t' || c == '\r' || c == '\n') continue;
                if (c == '=') break;
                int value = Base64Value(c);
                if (value < 0) throw new FormatException("The input is not a valid base-64 string.");
                accumulator = (accumulator << 6) | value;
                bits += 6;
                if (bits >= 8)
                {
                    bits -= 8;
                    buffer[count++] = (byte)((accumulator >> bits) & 0xFF);
                }
            }
            if (count == buffer.Length) return buffer;
            byte[] result = new byte[count];
            Array.Copy(buffer, result, count);
            return result;
        }

        private static int Base64Value(char c)
        {
            if (c >= 'A' && c <= 'Z') return c - 'A';
            if (c >= 'a' && c <= 'z') return c - 'a' + 26;
            if (c >= '0' && c <= '9') return c - '0' + 52;
            if (c == '+') return 62;
            if (c == '/') return 63;
            return -1;
        }

        public static byte ToByte(string value)
        {
            if ((object)value == null) return 0;
            return Byte.Parse(value);
        }

        public static sbyte ToSByte(string value)
        {
            if ((object)value == null) return 0;
            return SByte.Parse(value);
        }

        public static short ToInt16(string value)
        {
            if ((object)value == null) return 0;
            return Int16.Parse(value);
        }

        public static ushort ToUInt16(string value)
        {
            if ((object)value == null) return 0;
            return UInt16.Parse(value);
        }

        public static uint ToUInt32(string value)
        {
            if ((object)value == null) return 0;
            return UInt32.Parse(value);
        }

        public static ulong ToUInt64(string value)
        {
            if ((object)value == null) return 0;
            return UInt64.Parse(value);
        }

        public static char ToChar(string value)
        {
            if ((object)value == null) throw new ArgumentNullException("value");
            if (value.Length != 1) throw new FormatException("String must be exactly one character long.");
            return value[0];
        }

        public static string ToString(string value)
        {
            return value;
        }

        public static byte ToByte(string value, IFormatProvider provider) { return ToByte(value); }
        public static sbyte ToSByte(string value, IFormatProvider provider) { return SByte.Parse(value); }
        public static short ToInt16(string value, IFormatProvider provider) { return ToInt16(value); }
        public static ushort ToUInt16(string value, IFormatProvider provider) { return ToUInt16(value); }
        public static int ToInt32(string value, IFormatProvider provider) { return ToInt32(value); }
        public static uint ToUInt32(string value, IFormatProvider provider) { return ToUInt32(value); }
        public static long ToInt64(string value, IFormatProvider provider) { return ToInt64(value); }
        public static ulong ToUInt64(string value, IFormatProvider provider) { return ToUInt64(value); }

        public static string ToString(byte value, IFormatProvider provider) { return ToString(value); }
        public static string ToString(sbyte value, IFormatProvider provider) { return ToString(value); }
        public static string ToString(short value, IFormatProvider provider) { return ToString(value); }
        public static string ToString(ushort value, IFormatProvider provider) { return ToString(value); }
        public static string ToString(int value, IFormatProvider provider) { return ToString(value); }
        public static string ToString(uint value, IFormatProvider provider) { return ToString(value); }
        public static string ToString(long value, IFormatProvider provider) { return ToString(value); }
        public static string ToString(ulong value, IFormatProvider provider) { return ToString(value); }


        public static sbyte ToSByte(bool value)
        {
            return value ? (sbyte)1 : (sbyte)0;
        }
        public static sbyte ToSByte(char value)
        {
            if (value > 127) throw new OverflowException("Value was either too large or too small for a signed byte.");
            return (sbyte)value;
        }
        public static sbyte ToSByte(sbyte value)
        {
            return value;
        }
        public static sbyte ToSByte(byte value)
        {
            if (value > 127) throw new OverflowException("Value was either too large or too small for a signed byte.");
            return (sbyte)value;
        }
        public static sbyte ToSByte(short value)
        {
            if (value < -128 || value > 127) throw new OverflowException("Value was either too large or too small for a signed byte.");
            return (sbyte)value;
        }
        public static sbyte ToSByte(ushort value)
        {
            if (value > 127) throw new OverflowException("Value was either too large or too small for a signed byte.");
            return (sbyte)value;
        }
        public static sbyte ToSByte(int value)
        {
            if (value < -128 || value > 127) throw new OverflowException("Value was either too large or too small for a signed byte.");
            return (sbyte)value;
        }
        public static sbyte ToSByte(uint value)
        {
            if (value > 127) throw new OverflowException("Value was either too large or too small for a signed byte.");
            return (sbyte)value;
        }
        public static sbyte ToSByte(long value)
        {
            if (value < -128L || value > 127L) throw new OverflowException("Value was either too large or too small for a signed byte.");
            return (sbyte)value;
        }
        public static sbyte ToSByte(ulong value)
        {
            if (value > 127UL) throw new OverflowException("Value was either too large or too small for a signed byte.");
            return (sbyte)value;
        }

        public static byte ToByte(bool value)
        {
            return value ? (byte)1 : (byte)0;
        }
        public static byte ToByte(char value)
        {
            if (value > 255) throw new OverflowException("Value was either too large or too small for an unsigned byte.");
            return (byte)value;
        }
        public static byte ToByte(sbyte value)
        {
            if (value < 0) throw new OverflowException("Value was either too large or too small for an unsigned byte.");
            return (byte)value;
        }
        public static byte ToByte(byte value)
        {
            return value;
        }
        public static byte ToByte(short value)
        {
            if (value < 0 || value > 255) throw new OverflowException("Value was either too large or too small for an unsigned byte.");
            return (byte)value;
        }
        public static byte ToByte(ushort value)
        {
            if (value > 255) throw new OverflowException("Value was either too large or too small for an unsigned byte.");
            return (byte)value;
        }
        public static byte ToByte(uint value)
        {
            if (value > 255) throw new OverflowException("Value was either too large or too small for an unsigned byte.");
            return (byte)value;
        }
        public static byte ToByte(long value)
        {
            if (value < 0L || value > 255L) throw new OverflowException("Value was either too large or too small for an unsigned byte.");
            return (byte)value;
        }
        public static byte ToByte(ulong value)
        {
            if (value > 255UL) throw new OverflowException("Value was either too large or too small for an unsigned byte.");
            return (byte)value;
        }

        public static short ToInt16(bool value)
        {
            return value ? (short)1 : (short)0;
        }
        public static short ToInt16(char value)
        {
            if (value > 32767) throw new OverflowException("Value was either too large or too small for an Int16.");
            return (short)value;
        }
        public static short ToInt16(sbyte value)
        {
            return (short)value;
        }
        public static short ToInt16(byte value)
        {
            return (short)value;
        }
        public static short ToInt16(short value)
        {
            return value;
        }
        public static short ToInt16(ushort value)
        {
            if (value > 32767) throw new OverflowException("Value was either too large or too small for an Int16.");
            return (short)value;
        }
        public static short ToInt16(int value)
        {
            if (value < -32768 || value > 32767) throw new OverflowException("Value was either too large or too small for an Int16.");
            return (short)value;
        }
        public static short ToInt16(uint value)
        {
            if (value > 32767) throw new OverflowException("Value was either too large or too small for an Int16.");
            return (short)value;
        }
        public static short ToInt16(long value)
        {
            if (value < -32768L || value > 32767L) throw new OverflowException("Value was either too large or too small for an Int16.");
            return (short)value;
        }
        public static short ToInt16(ulong value)
        {
            if (value > 32767UL) throw new OverflowException("Value was either too large or too small for an Int16.");
            return (short)value;
        }

        public static ushort ToUInt16(bool value)
        {
            return value ? (ushort)1 : (ushort)0;
        }
        public static ushort ToUInt16(char value)
        {
            return (ushort)value;
        }
        public static ushort ToUInt16(sbyte value)
        {
            if (value < 0) throw new OverflowException("Value was either too large or too small for a UInt16.");
            return (ushort)value;
        }
        public static ushort ToUInt16(byte value)
        {
            return (ushort)value;
        }
        public static ushort ToUInt16(short value)
        {
            if (value < 0) throw new OverflowException("Value was either too large or too small for a UInt16.");
            return (ushort)value;
        }
        public static ushort ToUInt16(ushort value)
        {
            return value;
        }
        public static ushort ToUInt16(int value)
        {
            if (value < 0 || value > 65535) throw new OverflowException("Value was either too large or too small for a UInt16.");
            return (ushort)value;
        }
        public static ushort ToUInt16(uint value)
        {
            if (value > 65535) throw new OverflowException("Value was either too large or too small for a UInt16.");
            return (ushort)value;
        }
        public static ushort ToUInt16(long value)
        {
            if (value < 0L || value > 65535L) throw new OverflowException("Value was either too large or too small for a UInt16.");
            return (ushort)value;
        }
        public static ushort ToUInt16(ulong value)
        {
            if (value > 65535UL) throw new OverflowException("Value was either too large or too small for a UInt16.");
            return (ushort)value;
        }

        public static int ToInt32(bool value)
        {
            return value ? (int)1 : (int)0;
        }
        public static int ToInt32(char value)
        {
            return (int)value;
        }
        public static int ToInt32(sbyte value)
        {
            return (int)value;
        }
        public static int ToInt32(byte value)
        {
            return (int)value;
        }
        public static int ToInt32(short value)
        {
            return (int)value;
        }
        public static int ToInt32(ushort value)
        {
            return (int)value;
        }
        public static int ToInt32(int value)
        {
            return value;
        }
        public static int ToInt32(uint value)
        {
            if (value > 2147483647) throw new OverflowException("Value was either too large or too small for an Int32.");
            return (int)value;
        }
        public static int ToInt32(long value)
        {
            if (value < -2147483648L || value > 2147483647L) throw new OverflowException("Value was either too large or too small for an Int32.");
            return (int)value;
        }
        public static int ToInt32(ulong value)
        {
            if (value > 2147483647UL) throw new OverflowException("Value was either too large or too small for an Int32.");
            return (int)value;
        }

        public static uint ToUInt32(bool value)
        {
            return value ? (uint)1 : (uint)0;
        }
        public static uint ToUInt32(char value)
        {
            return (uint)value;
        }
        public static uint ToUInt32(sbyte value)
        {
            if (value < 0) throw new OverflowException("Value was either too large or too small for a UInt32.");
            return (uint)value;
        }
        public static uint ToUInt32(byte value)
        {
            return (uint)value;
        }
        public static uint ToUInt32(short value)
        {
            if (value < 0) throw new OverflowException("Value was either too large or too small for a UInt32.");
            return (uint)value;
        }
        public static uint ToUInt32(ushort value)
        {
            return (uint)value;
        }
        public static uint ToUInt32(int value)
        {
            if (value < 0) throw new OverflowException("Value was either too large or too small for a UInt32.");
            return (uint)value;
        }
        public static uint ToUInt32(uint value)
        {
            return value;
        }
        public static uint ToUInt32(long value)
        {
            if (value < 0L || value > 4294967295L) throw new OverflowException("Value was either too large or too small for a UInt32.");
            return (uint)value;
        }
        public static uint ToUInt32(ulong value)
        {
            if (value > 4294967295UL) throw new OverflowException("Value was either too large or too small for a UInt32.");
            return (uint)value;
        }

        public static long ToInt64(bool value)
        {
            return value ? (long)1 : (long)0;
        }
        public static long ToInt64(char value)
        {
            return (long)value;
        }
        public static long ToInt64(sbyte value)
        {
            return (long)value;
        }
        public static long ToInt64(byte value)
        {
            return (long)value;
        }
        public static long ToInt64(short value)
        {
            return (long)value;
        }
        public static long ToInt64(ushort value)
        {
            return (long)value;
        }
        public static long ToInt64(int value)
        {
            return (long)value;
        }
        public static long ToInt64(uint value)
        {
            return (long)value;
        }
        public static long ToInt64(long value)
        {
            return value;
        }
        public static long ToInt64(ulong value)
        {
            if (value > 9223372036854775807UL) throw new OverflowException("Value was either too large or too small for an Int64.");
            return (long)value;
        }

        public static ulong ToUInt64(bool value)
        {
            return value ? (ulong)1 : (ulong)0;
        }
        public static ulong ToUInt64(char value)
        {
            return (ulong)value;
        }
        public static ulong ToUInt64(sbyte value)
        {
            if (value < 0) throw new OverflowException("Value was either too large or too small for a UInt64.");
            return (ulong)value;
        }
        public static ulong ToUInt64(byte value)
        {
            return (ulong)value;
        }
        public static ulong ToUInt64(short value)
        {
            if (value < 0) throw new OverflowException("Value was either too large or too small for a UInt64.");
            return (ulong)value;
        }
        public static ulong ToUInt64(ushort value)
        {
            return (ulong)value;
        }
        public static ulong ToUInt64(int value)
        {
            if (value < 0) throw new OverflowException("Value was either too large or too small for a UInt64.");
            return (ulong)value;
        }
        public static ulong ToUInt64(uint value)
        {
            return (ulong)value;
        }
        public static ulong ToUInt64(long value)
        {
            if (value < 0L) throw new OverflowException("Value was either too large or too small for a UInt64.");
            return (ulong)value;
        }
        public static ulong ToUInt64(ulong value)
        {
            return value;
        }

        public static char ToChar(char value)
        {
            return value;
        }
        public static char ToChar(sbyte value)
        {
            if (value < 0) throw new OverflowException("Value was either too large or too small for a character.");
            return (char)value;
        }
        public static char ToChar(byte value)
        {
            return (char)value;
        }
        public static char ToChar(short value)
        {
            if (value < 0) throw new OverflowException("Value was either too large or too small for a character.");
            return (char)value;
        }
        public static char ToChar(ushort value)
        {
            return (char)value;
        }
        public static char ToChar(uint value)
        {
            if (value > 65535) throw new OverflowException("Value was either too large or too small for a character.");
            return (char)value;
        }
        public static char ToChar(long value)
        {
            if (value < 0L || value > 65535L) throw new OverflowException("Value was either too large or too small for a character.");
            return (char)value;
        }
        public static char ToChar(ulong value)
        {
            if (value > 65535UL) throw new OverflowException("Value was either too large or too small for a character.");
            return (char)value;
        }

        public static char ToChar(bool value)
        {
            throw new InvalidCastException("Invalid cast from 'Boolean' to 'Char'.");
        }

        public static bool ToBoolean(char value)
        {
            throw new InvalidCastException("Invalid cast from 'Char' to 'Boolean'.");
        }

        public static bool ToBoolean(bool value)
        {
            return value;
        }
        public static bool ToBoolean(sbyte value)
        {
            return value != 0;
        }
        public static bool ToBoolean(byte value)
        {
            return value != 0;
        }
        public static bool ToBoolean(short value)
        {
            return value != 0;
        }
        public static bool ToBoolean(ushort value)
        {
            return value != 0;
        }
        public static bool ToBoolean(uint value)
        {
            return value != 0;
        }
        public static bool ToBoolean(long value)
        {
            return value != 0;
        }
        public static bool ToBoolean(ulong value)
        {
            return value != 0;
        }

        public static string ToString(char value)
        {
            return value.ToString();
        }
        public static string ToString(sbyte value)
        {
            return value.ToString();
        }
        public static string ToString(byte value)
        {
            return value.ToString();
        }
        public static string ToString(short value)
        {
            return value.ToString();
        }
        public static string ToString(ushort value)
        {
            return value.ToString();
        }
        public static string ToString(uint value)
        {
            return value.ToString();
        }
        public static string ToString(ulong value)
        {
            return value.ToString();
        }

#if LAMELLA_SURFACE_FLOAT

        private static double RoundToEven(double value)
        {
            if (value != value) return value;
            if (value >= 4503599627370496.0 || value <= -4503599627370496.0) return value;
            if (value >= 0.0)
            {
                long t = (long)value;
                double dif = value - t;
                if (dif > 0.5 || (dif == 0.5 && (t & 1L) != 0L)) t++;
                return (double)t;
            }
            else
            {
                long t = (long)value;
                double dif = value - t;
                if (dif < -0.5 || (dif == -0.5 && (t & 1L) != 0L)) t--;
                return (double)t;
            }
        }

        public static byte ToByte(double value)
        {
            double r = RoundToEven(value);
            if (r != r || r < 0.0 || r > 255.0) throw new OverflowException("Value was either too large or too small for an unsigned byte.");
            return (byte)r;
        }

        public static sbyte ToSByte(double value)
        {
            double r = RoundToEven(value);
            if (r != r || r < -128.0 || r > 127.0) throw new OverflowException("Value was either too large or too small for a signed byte.");
            return (sbyte)r;
        }

        public static short ToInt16(double value)
        {
            double r = RoundToEven(value);
            if (r != r || r < -32768.0 || r > 32767.0) throw new OverflowException("Value was either too large or too small for an Int16.");
            return (short)r;
        }

        public static ushort ToUInt16(double value)
        {
            double r = RoundToEven(value);
            if (r != r || r < 0.0 || r > 65535.0) throw new OverflowException("Value was either too large or too small for a UInt16.");
            return (ushort)r;
        }

        public static int ToInt32(double value)
        {
            double r = RoundToEven(value);
            if (r != r || r < -2147483648.0 || r > 2147483647.0) throw new OverflowException("Value was either too large or too small for an Int32.");
            return (int)r;
        }

        public static uint ToUInt32(double value)
        {
            double r = RoundToEven(value);
            if (r != r || r < 0.0 || r > 4294967295.0) throw new OverflowException("Value was either too large or too small for a UInt32.");
            return (uint)r;
        }

        public static long ToInt64(double value)
        {
            double r = RoundToEven(value);
            if (r != r || r < -9223372036854775808.0 || r >= 9223372036854775808.0) throw new OverflowException("Value was either too large or too small for an Int64.");
            return (long)r;
        }

        public static ulong ToUInt64(double value)
        {
            double r = RoundToEven(value);
            if (r != r || r < 0.0 || r >= 18446744073709551616.0) throw new OverflowException("Value was either too large or too small for a UInt64.");
            return (ulong)r;
        }

        public static byte ToByte(float value) { return ToByte((double)value); }
        public static sbyte ToSByte(float value) { return ToSByte((double)value); }
        public static short ToInt16(float value) { return ToInt16((double)value); }
        public static ushort ToUInt16(float value) { return ToUInt16((double)value); }
        public static int ToInt32(float value) { return ToInt32((double)value); }
        public static uint ToUInt32(float value) { return ToUInt32((double)value); }
        public static long ToInt64(float value) { return ToInt64((double)value); }
        public static ulong ToUInt64(float value) { return ToUInt64((double)value); }

        public static double ToDouble(bool value) { return value ? (double)1 : (double)0; }
        public static double ToDouble(byte value) { return (double)value; }
        public static double ToDouble(sbyte value) { return (double)value; }
        public static double ToDouble(short value) { return (double)value; }
        public static double ToDouble(ushort value) { return (double)value; }
        public static double ToDouble(int value) { return (double)value; }
        public static double ToDouble(uint value) { return (double)value; }
        public static double ToDouble(long value) { return (double)value; }
        public static double ToDouble(ulong value) { return (double)value; }
        public static double ToDouble(float value) { return (double)value; }
        public static double ToDouble(double value) { return (double)value; }

        public static float ToSingle(bool value) { return value ? (float)1 : (float)0; }
        public static float ToSingle(byte value) { return (float)value; }
        public static float ToSingle(sbyte value) { return (float)value; }
        public static float ToSingle(short value) { return (float)value; }
        public static float ToSingle(ushort value) { return (float)value; }
        public static float ToSingle(int value) { return (float)value; }
        public static float ToSingle(uint value) { return (float)value; }
        public static float ToSingle(long value) { return (float)value; }
        public static float ToSingle(ulong value) { return (float)value; }
        public static float ToSingle(float value) { return (float)value; }
        public static float ToSingle(double value) { return (float)value; }

        public static double ToDouble(string value)
        {
            if ((object)value == null) return 0.0;
            return Double.Parse(value);
        }

        public static float ToSingle(string value)
        {
            if ((object)value == null) return 0.0f;
            return Single.Parse(value);
        }

        public static double ToDouble(string value, IFormatProvider provider) { return ToDouble(value); }
        public static float ToSingle(string value, IFormatProvider provider) { return ToSingle(value); }

        public static bool ToBoolean(double value) { return value != 0.0; }
        public static bool ToBoolean(float value) { return value != 0.0f; }

        public static string ToString(double value) { return value.ToString(); }
        public static string ToString(float value) { return value.ToString(); }
        public static string ToString(double value, IFormatProvider provider) { return value.ToString(); }
        public static string ToString(float value, IFormatProvider provider) { return value.ToString(); }
#endif

#if LAMELLA_SURFACE_DECIMAL

        public static int ToInt32(Decimal value) { return (int)Decimal.Round(value, 0); }
        public static long ToInt64(Decimal value) { return (long)Decimal.Round(value, 0); }

        public static byte ToByte(Decimal value)
        {
            long r = (long)Decimal.Round(value, 0);
            if (r < 0L || r > 255L) throw new OverflowException("Value was either too large or too small for an unsigned byte.");
            return (byte)r;
        }

        public static sbyte ToSByte(Decimal value)
        {
            long r = (long)Decimal.Round(value, 0);
            if (r < -128L || r > 127L) throw new OverflowException("Value was either too large or too small for a signed byte.");
            return (sbyte)r;
        }

        public static short ToInt16(Decimal value)
        {
            long r = (long)Decimal.Round(value, 0);
            if (r < -32768L || r > 32767L) throw new OverflowException("Value was either too large or too small for an Int16.");
            return (short)r;
        }

        public static ushort ToUInt16(Decimal value)
        {
            long r = (long)Decimal.Round(value, 0);
            if (r < 0L || r > 65535L) throw new OverflowException("Value was either too large or too small for a UInt16.");
            return (ushort)r;
        }

        public static uint ToUInt32(Decimal value)
        {
            long r = (long)Decimal.Round(value, 0);
            if (r < 0L || r > 4294967295L) throw new OverflowException("Value was either too large or too small for a UInt32.");
            return (uint)r;
        }

        public static ulong ToUInt64(Decimal value)
        {
            Decimal r = Decimal.Round(value, 0);
            if (r < Decimal.Zero) throw new OverflowException("Value was either too large or too small for a UInt64.");
            Decimal twoPow63 = new Decimal(9223372036854775808UL);
            if (r < twoPow63) return (ulong)(long)r;
            Decimal shifted = r - twoPow63;
            if (shifted >= twoPow63) throw new OverflowException("Value was either too large or too small for a UInt64.");
            return (ulong)(long)shifted + 9223372036854775808UL;
        }

        public static Decimal ToDecimal(bool value) { return value ? Decimal.One : Decimal.Zero; }
        public static Decimal ToDecimal(byte value) { return new Decimal((int)value); }
        public static Decimal ToDecimal(sbyte value) { return new Decimal((int)value); }
        public static Decimal ToDecimal(short value) { return new Decimal((int)value); }
        public static Decimal ToDecimal(ushort value) { return new Decimal((int)value); }
        public static Decimal ToDecimal(int value) { return new Decimal(value); }
        public static Decimal ToDecimal(uint value) { return new Decimal(value); }
        public static Decimal ToDecimal(long value) { return new Decimal(value); }
        public static Decimal ToDecimal(ulong value) { return new Decimal(value); }
        public static Decimal ToDecimal(Decimal value) { return value; }

        public static Decimal ToDecimal(string value)
        {
            if ((object)value == null) return Decimal.Zero;
            return Decimal.Parse(value);
        }

        public static Decimal ToDecimal(string value, IFormatProvider provider) { return ToDecimal(value); }

        public static bool ToBoolean(Decimal value) { return value != Decimal.Zero; }
        public static string ToString(Decimal value) { return value.ToString(); }
        public static string ToString(Decimal value, IFormatProvider provider) { return value.ToString(); }

#if LAMELLA_SURFACE_FLOAT
        public static Decimal ToDecimal(double value) { return new Decimal(value); }
        public static Decimal ToDecimal(float value) { return new Decimal((double)value); }
        public static double ToDouble(Decimal value) { return (double)value; }
        public static float ToSingle(Decimal value) { return (float)(double)value; }
#endif
#endif
    }
}
