// Lamella managed corlib (from scratch). -- System.Buffer
namespace System
{
    public static class Buffer
    {
        [Lamella.Runtime.RuntimeProvided] private static int ByteLengthInternal(Array array) { return -1; }

        [Lamella.Runtime.RuntimeProvided] private static void BlockCopyInternal(Array src, int srcOffset, Array dst, int dstOffset, int count) { }

        public static int ByteLength(Array array)
        {
            if ((object)array == null) throw new ArgumentNullException("array");
            int length = ByteLengthInternal(array);
            if (length < 0) throw new ArgumentException("Object must be an array of primitives.");
            return length;
        }

        public static void BlockCopy(Array src, int srcOffset, Array dst, int dstOffset, int count)
        {
            if ((object)src == null) throw new ArgumentNullException("src");
            if ((object)dst == null) throw new ArgumentNullException("dst");
            if (srcOffset < 0) throw new ArgumentOutOfRangeException("srcOffset");
            if (dstOffset < 0) throw new ArgumentOutOfRangeException("dstOffset");
            if (count < 0) throw new ArgumentOutOfRangeException("count");

            int srcLength = ByteLength(src);
            int dstLength = ByteLength(dst);

            if (srcOffset > srcLength - count) throw new ArgumentException("Offset and length were out of bounds.");
            if (dstOffset > dstLength - count) throw new ArgumentException("Offset and length were out of bounds.");

            BlockCopyInternal(src, srcOffset, dst, dstOffset, count);
        }
    }
}
