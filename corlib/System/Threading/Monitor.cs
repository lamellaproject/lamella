// Lamella managed corlib (from scratch). -- System.Threading.Monitor
#if LAMELLA_SURFACE_THREADS
namespace System.Threading
{
    public sealed class Monitor
    {
        private Monitor() { }

        private static void RequireObject(object obj)
        {
            if (obj == null) throw new ArgumentNullException("obj");
        }

        public static void Enter(object obj) { RequireObject(obj); EnterLock(obj); }

        public static void Enter(object obj, ref bool lockTaken)
        {
            RequireObject(obj);
            EnterLock(obj);
            lockTaken = true;
        }

        public static void Exit(object obj) { RequireObject(obj); ExitLock(obj); }

        public static bool TryEnter(object obj) { RequireObject(obj); return TryEnterLock(obj); }

        public static bool TryEnter(object obj, int millisecondsTimeout)
        {
            RequireObject(obj);
            if (millisecondsTimeout == Timeout.Infinite) { EnterLock(obj); return true; }
            if (millisecondsTimeout < 0) throw new ArgumentOutOfRangeException("millisecondsTimeout");
            if (TryEnterLock(obj)) return true;
            if (millisecondsTimeout == 0) return false;
            TryEnterLockTimeout(obj, millisecondsTimeout);
            return !WaitTimedOut();
        }

        public static bool Wait(object obj) { RequireObject(obj); WaitLock(obj); return true; }

        public static bool Wait(object obj, int millisecondsTimeout)
        {
            RequireObject(obj);
            if (millisecondsTimeout == Timeout.Infinite)
            {
                WaitLock(obj);
                return true;
            }
            if (millisecondsTimeout < 0) throw new ArgumentOutOfRangeException("millisecondsTimeout");
            WaitLockTimeout(obj, millisecondsTimeout);
            return !WaitTimedOut();
        }

        private static int TimeoutMilliseconds(TimeSpan timeout)
        {
            long milliseconds = timeout.Ticks / TimeSpan.TicksPerMillisecond;
            if (milliseconds == Timeout.Infinite) return Timeout.Infinite;
            if (milliseconds < 0 || milliseconds > Int32.MaxValue)
            {
                throw new ArgumentOutOfRangeException("timeout");
            }
            return (int)milliseconds;
        }

        public static bool TryEnter(object obj, TimeSpan timeout)
        {
            RequireObject(obj);
            return TryEnter(obj, TimeoutMilliseconds(timeout));
        }

        public static bool Wait(object obj, TimeSpan timeout)
        {
            RequireObject(obj);
            return Wait(obj, TimeoutMilliseconds(timeout));
        }

        public static void Pulse(object obj) { RequireObject(obj); PulseLock(obj); }

        public static void PulseAll(object obj) { RequireObject(obj); PulseAllLock(obj); }

        [Lamella.Runtime.RuntimeProvided] private static void EnterLock(object obj) { }
        [Lamella.Runtime.RuntimeProvided] private static void ExitLock(object obj) { }
        [Lamella.Runtime.RuntimeProvided] private static bool TryEnterLock(object obj) { return false; }
        [Lamella.Runtime.RuntimeProvided] private static void WaitLock(object obj) { }
        [Lamella.Runtime.RuntimeProvided] private static void WaitLockTimeout(object obj, int millisecondsTimeout) { }
        [Lamella.Runtime.RuntimeProvided] private static void TryEnterLockTimeout(object obj, int millisecondsTimeout) { }
        [Lamella.Runtime.RuntimeProvided] private static bool WaitTimedOut() { return false; }
        [Lamella.Runtime.RuntimeProvided] private static void PulseLock(object obj) { }
        [Lamella.Runtime.RuntimeProvided] private static void PulseAllLock(object obj) { }
    }
}
#endif
