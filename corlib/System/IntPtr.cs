// Lamella managed corlib (from scratch). -- System.IntPtr / System.UIntPtr
namespace System
{
    public struct IntPtr
    {
        public static readonly IntPtr Zero = FromRawValue(0L);

        public static int Size { get { return 8; } }

        public int ToInt32() { return (int)ToRawValue(this); }
        public long ToInt64() { return ToRawValue(this); }

        [Lamella.Runtime.RuntimeProvided] internal static IntPtr FromRawValue(long value) { return default(IntPtr); }
        [Lamella.Runtime.RuntimeProvided] internal static long ToRawValue(IntPtr value) { return 0; }
    }

    public struct UIntPtr { }
}
