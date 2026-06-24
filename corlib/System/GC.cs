// Lamella managed corlib (from scratch). -- System.GC
namespace System
{
    public static class GC
    {
        [Lamella.Runtime.RuntimeProvided] public static void Collect() { }

        [Lamella.Runtime.RuntimeProvided] public static void SuppressFinalize(object obj) { }

        [Lamella.Runtime.RuntimeProvided] public static void ReRegisterForFinalize(object obj) { }

        [Lamella.Runtime.RuntimeProvided] public static void WaitForPendingFinalizers() { }
    }
}
