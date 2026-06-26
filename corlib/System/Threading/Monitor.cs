// Lamella managed corlib (from scratch). -- System.Threading.Monitor
#if LAMELLA_SURFACE_THREADS
namespace System.Threading
{
    public static class Monitor
    {
        public static void Enter(object obj) { EnterLock(obj); }

        public static void Enter(object obj, ref bool lockTaken) { EnterLock(obj); lockTaken = true; }

        public static void Exit(object obj) { ExitLock(obj); }

        public static bool TryEnter(object obj) { return TryEnterLock(obj); }

        public static bool TryEnter(object obj, int millisecondsTimeout) { return TryEnterLock(obj); }

        public static bool Wait(object obj) { WaitLock(obj); return true; }

        public static bool Wait(object obj, int millisecondsTimeout) { WaitLock(obj); return true; }

        public static void Pulse(object obj) { PulseLock(obj); }

        public static void PulseAll(object obj) { PulseAllLock(obj); }

        [Lamella.Runtime.RuntimeProvided] private static void EnterLock(object obj) { }
        [Lamella.Runtime.RuntimeProvided] private static void ExitLock(object obj) { }
        [Lamella.Runtime.RuntimeProvided] private static bool TryEnterLock(object obj) { return false; }
        [Lamella.Runtime.RuntimeProvided] private static void WaitLock(object obj) { }
        [Lamella.Runtime.RuntimeProvided] private static void PulseLock(object obj) { }
        [Lamella.Runtime.RuntimeProvided] private static void PulseAllLock(object obj) { }
    }
}
#endif
