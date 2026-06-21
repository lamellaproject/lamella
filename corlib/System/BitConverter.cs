// Lamella managed corlib (from scratch). -- System.BitConverter
namespace System
{
    public sealed class BitConverter
    {
        public static readonly bool IsLittleEndian = true;

        [Lamella.Runtime.RuntimeProvided] public static long DoubleToInt64Bits(double value) { return 0; }
        [Lamella.Runtime.RuntimeProvided] public static double Int64BitsToDouble(long value) { return 0; }
        [Lamella.Runtime.RuntimeProvided] public static int SingleToInt32Bits(float value) { return 0; }
        [Lamella.Runtime.RuntimeProvided] public static float Int32BitsToSingle(int value) { return 0; }

        public static byte[] GetBytes(bool value)
        {
            byte[] bytes = new byte[1];
            bytes[0] = (byte)(value ? 1 : 0);
            return bytes;
        }

        public static byte[] GetBytes(char value)
        {
            return GetBytes((short)value);
        }

        public static byte[] GetBytes(short value)
        {
            byte[] bytes = new byte[2];
            bytes[0] = (byte)value;
            bytes[1] = (byte)(value >> 8);
            return bytes;
        }

        public static byte[] GetBytes(ushort value)
        {
            return GetBytes((short)value);
        }

        public static byte[] GetBytes(int value)
        {
            byte[] bytes = new byte[4];
            bytes[0] = (byte)value;
            bytes[1] = (byte)(value >> 8);
            bytes[2] = (byte)(value >> 16);
            bytes[3] = (byte)(value >> 24);
            return bytes;
        }

        public static byte[] GetBytes(uint value)
        {
            return GetBytes((int)value);
        }

        public static byte[] GetBytes(long value)
        {
            byte[] bytes = new byte[8];
            bytes[0] = (byte)value;
            bytes[1] = (byte)(value >> 8);
            bytes[2] = (byte)(value >> 16);
            bytes[3] = (byte)(value >> 24);
            bytes[4] = (byte)(value >> 32);
            bytes[5] = (byte)(value >> 40);
            bytes[6] = (byte)(value >> 48);
            bytes[7] = (byte)(value >> 56);
            return bytes;
        }

        public static byte[] GetBytes(ulong value)
        {
            return GetBytes((long)value);
        }

        public static byte[] GetBytes(float value)
        {
            return GetBytes(SingleToInt32Bits(value));
        }

        public static byte[] GetBytes(double value)
        {
            return GetBytes(DoubleToInt64Bits(value));
        }

        public static short ToInt16(byte[] value, int startIndex)
        {
            return (short)(value[startIndex] | (value[startIndex + 1] << 8));
        }

        public static ushort ToUInt16(byte[] value, int startIndex)
        {
            return (ushort)ToInt16(value, startIndex);
        }

        public static int ToInt32(byte[] value, int startIndex)
        {
            return value[startIndex]
                | (value[startIndex + 1] << 8)
                | (value[startIndex + 2] << 16)
                | (value[startIndex + 3] << 24);
        }

        public static uint ToUInt32(byte[] value, int startIndex)
        {
            return (uint)ToInt32(value, startIndex);
        }

        public static long ToInt64(byte[] value, int startIndex)
        {
            return (long)value[startIndex]
                | ((long)value[startIndex + 1] << 8)
                | ((long)value[startIndex + 2] << 16)
                | ((long)value[startIndex + 3] << 24)
                | ((long)value[startIndex + 4] << 32)
                | ((long)value[startIndex + 5] << 40)
                | ((long)value[startIndex + 6] << 48)
                | ((long)value[startIndex + 7] << 56);
        }

        public static ulong ToUInt64(byte[] value, int startIndex)
        {
            return (ulong)ToInt64(value, startIndex);
        }

        public static bool ToBoolean(byte[] value, int startIndex)
        {
            return value[startIndex] != 0;
        }

        public static char ToChar(byte[] value, int startIndex)
        {
            return (char)ToUInt16(value, startIndex);
        }

        public static float ToSingle(byte[] value, int startIndex)
        {
            return Int32BitsToSingle(ToInt32(value, startIndex));
        }

        public static double ToDouble(byte[] value, int startIndex)
        {
            return Int64BitsToDouble(ToInt64(value, startIndex));
        }

        public static string ToString(byte[] value)
        {
            if ((object)value == null) throw new ArgumentNullException("value");
            if (value.Length == 0) return "";
            System.Text.StringBuilder result = new System.Text.StringBuilder();
            for (int i = 0; i < value.Length; i++)
            {
                if (i > 0) result.Append('-');
                int b = value[i];
                result.Append(HexDigit(b >> 4));
                result.Append(HexDigit(b & 0xF));
            }
            return result.ToString();
        }

        private static char HexDigit(int nibble)
        {
            if (nibble < 10) return (char)('0' + nibble);
            return (char)('A' + (nibble - 10));
        }
    }
}
