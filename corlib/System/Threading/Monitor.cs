// Lamella managed corlib (from scratch). -- System.Threading.Monitor
namespace System.Threading
{
    public static class Monitor
    {
        public static void Enter(object obj) { }

        public static void Enter(object obj, ref bool lockTaken) { lockTaken = true; }

        public static void Exit(object obj) { }

        public static bool TryEnter(object obj) { return true; }

        public static bool TryEnter(object obj, int millisecondsTimeout) { return true; }

        public static void Pulse(object obj) { }

        public static void PulseAll(object obj) { }
    }
}
