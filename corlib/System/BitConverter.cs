// Lamella managed corlib (from scratch). -- System.BitConverter
namespace System
{
    public sealed class BitConverter
    {
        public static readonly bool IsLittleEndian = true;

        public static byte[] GetBytes(short value)
        {
            byte[] bytes = new byte[2];
            bytes[0] = (byte)value;
            bytes[1] = (byte)(value >> 8);
            return bytes;
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

        public static short ToInt16(byte[] value, int startIndex)
        {
            return (short)(value[startIndex] | (value[startIndex + 1] << 8));
        }

        public static int ToInt32(byte[] value, int startIndex)
        {
            return value[startIndex]
                | (value[startIndex + 1] << 8)
                | (value[startIndex + 2] << 16)
                | (value[startIndex + 3] << 24);
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
    }
}
