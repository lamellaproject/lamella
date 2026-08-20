// Lamella managed corlib (from scratch). -- System.Runtime.CompilerServices.MethodImplOptions
namespace System.Runtime.CompilerServices
{
    public enum MethodImplOptions
    {
        Unmanaged = 4,
        NoInlining = 8,
        ForwardRef = 16,
        Synchronized = 32,
        PreserveSig = 128,
        InternalCall = 4096
    }
}
