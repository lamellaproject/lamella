// Lamella managed corlib (from scratch). -- System.IntPtr / System.UIntPtr
namespace System
{
    public struct IntPtr
    {
        public static readonly IntPtr Zero = FromRawValue(0L);

        public static int Size { get { return 8; } }

        public int ToInt32() { return (int)ToRawValue(this); }
        public long ToInt64() { return ToRawValue(this); }

        public IntPtr(int value) { this = FromRawValue(value); }
        public IntPtr(long value) { this = FromRawValue(value); }

        public override bool Equals(object obj)
        {
            if (obj is IntPtr) return ToRawValue(this) == ToRawValue((IntPtr)obj);
            return false;
        }

        public override int GetHashCode()
        {
            long raw = ToRawValue(this);
            return unchecked((int)raw) ^ (int)(raw >> 32);
        }

        public static bool operator ==(IntPtr value1, IntPtr value2) { return ToRawValue(value1) == ToRawValue(value2); }
        public static bool operator !=(IntPtr value1, IntPtr value2) { return ToRawValue(value1) != ToRawValue(value2); }

        public static explicit operator IntPtr(int value) { return FromRawValue(value); }
        public static explicit operator IntPtr(long value) { return FromRawValue(value); }
        public static explicit operator int(IntPtr value) { return value.ToInt32(); }
        public static explicit operator long(IntPtr value) { return value.ToInt64(); }

        public override string ToString() { return ToRawValue(this).ToString(); }

        [Lamella.Runtime.RuntimeProvided] internal static IntPtr FromRawValue(long value) { return new IntPtr(); }
        [Lamella.Runtime.RuntimeProvided] internal static long ToRawValue(IntPtr value) { return 0; }
    }

    public struct UIntPtr
    {
        public static readonly UIntPtr Zero = FromRawValue(0);

        public static int Size { get { return 8; } }

        public uint ToUInt32() { return (uint)ToRawValue(this); }
        public ulong ToUInt64() { return ToRawValue(this); }

        public UIntPtr(uint value) { this = FromRawValue(value); }
        public UIntPtr(ulong value) { this = FromRawValue(value); }

        public override bool Equals(object obj)
        {
            if (obj is UIntPtr) return ToRawValue(this) == ToRawValue((UIntPtr)obj);
            return false;
        }

        public override int GetHashCode()
        {
            ulong raw = ToRawValue(this);
            return unchecked((int)raw) ^ (int)(raw >> 32);
        }

        public static bool operator ==(UIntPtr value1, UIntPtr value2) { return ToRawValue(value1) == ToRawValue(value2); }
        public static bool operator !=(UIntPtr value1, UIntPtr value2) { return ToRawValue(value1) != ToRawValue(value2); }

        public static explicit operator UIntPtr(uint value) { return FromRawValue(value); }
        public static explicit operator UIntPtr(ulong value) { return FromRawValue(value); }
        public static explicit operator uint(UIntPtr value) { return value.ToUInt32(); }
        public static explicit operator ulong(UIntPtr value) { return value.ToUInt64(); }

        public override string ToString() { return ToRawValue(this).ToString(); }

        [Lamella.Runtime.RuntimeProvided] internal static UIntPtr FromRawValue(ulong value) { return new UIntPtr(); }
        [Lamella.Runtime.RuntimeProvided] internal static ulong ToRawValue(UIntPtr value) { return 0; }
    }
}
