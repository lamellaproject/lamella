// Lamella managed corlib (from scratch). -- System.Runtime.InteropServices.Marshal
namespace System.Runtime.InteropServices
{
    public sealed class Marshal
    {
        private Marshal() { }

        public static System.IntPtr AllocHGlobal(int cb) { return System.IntPtr.FromRawValue(__AllocHGlobal(cb)); }
        public static System.IntPtr AllocHGlobal(System.IntPtr cb) { return System.IntPtr.FromRawValue(__AllocHGlobal(System.IntPtr.ToRawValue(cb))); }
        public static void FreeHGlobal(System.IntPtr hglobal) { __FreeHGlobal(System.IntPtr.ToRawValue(hglobal)); }

        public static byte ReadByte(System.IntPtr ptr) { return (byte)__ReadByte(System.IntPtr.ToRawValue(ptr)); }
        public static byte ReadByte(System.IntPtr ptr, int ofs) { return (byte)__ReadByte(System.IntPtr.ToRawValue(ptr) + ofs); }
        public static short ReadInt16(System.IntPtr ptr) { return (short)__ReadInt16(System.IntPtr.ToRawValue(ptr)); }
        public static short ReadInt16(System.IntPtr ptr, int ofs) { return (short)__ReadInt16(System.IntPtr.ToRawValue(ptr) + ofs); }
        public static int ReadInt32(System.IntPtr ptr) { return __ReadInt32(System.IntPtr.ToRawValue(ptr)); }
        public static int ReadInt32(System.IntPtr ptr, int ofs) { return __ReadInt32(System.IntPtr.ToRawValue(ptr) + ofs); }
        public static long ReadInt64(System.IntPtr ptr) { return __ReadInt64(System.IntPtr.ToRawValue(ptr)); }
        public static long ReadInt64(System.IntPtr ptr, int ofs) { return __ReadInt64(System.IntPtr.ToRawValue(ptr) + ofs); }
        public static System.IntPtr ReadIntPtr(System.IntPtr ptr) { return System.IntPtr.FromRawValue(__ReadInt64(System.IntPtr.ToRawValue(ptr))); }

        public static void WriteByte(System.IntPtr ptr, byte val) { __WriteByte(System.IntPtr.ToRawValue(ptr), val); }
        public static void WriteByte(System.IntPtr ptr, int ofs, byte val) { __WriteByte(System.IntPtr.ToRawValue(ptr) + ofs, val); }
        public static void WriteInt16(System.IntPtr ptr, short val) { __WriteInt16(System.IntPtr.ToRawValue(ptr), val); }
        public static void WriteInt16(System.IntPtr ptr, int ofs, short val) { __WriteInt16(System.IntPtr.ToRawValue(ptr) + ofs, val); }
        public static void WriteInt32(System.IntPtr ptr, int val) { __WriteInt32(System.IntPtr.ToRawValue(ptr), val); }
        public static void WriteInt32(System.IntPtr ptr, int ofs, int val) { __WriteInt32(System.IntPtr.ToRawValue(ptr) + ofs, val); }
        public static void WriteInt64(System.IntPtr ptr, long val) { __WriteInt64(System.IntPtr.ToRawValue(ptr), val); }
        public static void WriteInt64(System.IntPtr ptr, int ofs, long val) { __WriteInt64(System.IntPtr.ToRawValue(ptr) + ofs, val); }
        public static void WriteIntPtr(System.IntPtr ptr, System.IntPtr val) { __WriteInt64(System.IntPtr.ToRawValue(ptr), System.IntPtr.ToRawValue(val)); }

        [Lamella.Runtime.RuntimeProvided] public static int SizeOf(System.Type t) { return 0; }

        [Lamella.Runtime.RuntimeProvided] internal static long __AllocHGlobal(long size) { return 0; }
        [Lamella.Runtime.RuntimeProvided] internal static void __FreeHGlobal(long ptr) { }
        [Lamella.Runtime.RuntimeProvided] internal static int __ReadByte(long ptr) { return 0; }
        [Lamella.Runtime.RuntimeProvided] internal static int __ReadInt16(long ptr) { return 0; }
        [Lamella.Runtime.RuntimeProvided] internal static int __ReadInt32(long ptr) { return 0; }
        [Lamella.Runtime.RuntimeProvided] internal static long __ReadInt64(long ptr) { return 0; }
        [Lamella.Runtime.RuntimeProvided] internal static void __WriteByte(long ptr, int val) { }
        [Lamella.Runtime.RuntimeProvided] internal static void __WriteInt16(long ptr, int val) { }
        [Lamella.Runtime.RuntimeProvided] internal static void __WriteInt32(long ptr, int val) { }
        [Lamella.Runtime.RuntimeProvided] internal static void __WriteInt64(long ptr, long val) { }
    }
}
