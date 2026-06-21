// Lamella managed corlib (from scratch). -- System.Convert
namespace System
{
    public sealed class Convert
    {
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
    }
}
