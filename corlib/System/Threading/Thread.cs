// Lamella managed corlib (from scratch). -- System.Threading.Thread
#if LAMELLA_SURFACE_THREADS
namespace System.Threading
{
    public sealed class Thread
    {
        private static Thread _current = new Thread();
        private ThreadStart _start;
        private int _id;
        private string _name;
        private bool _isBackground;
        private ThreadPriority _priority = ThreadPriority.Normal;

        public Thread(ThreadStart start) { _start = start; }

        internal Thread() { }

        public void Start()
        {
            int id = StartThread(_start, _isBackground);
            if (id < 0) throw new OutOfMemoryException("Cannot start another thread.");
            _id = id;
        }

        public bool IsBackground
        {
            get { return _isBackground; }
            set { _isBackground = value; }
        }

        public void Join() { if (_id != 0) JoinThread(_id); }

        public bool Join(int millisecondsTimeout)
        {
            if (millisecondsTimeout < Timeout.Infinite) throw new ArgumentOutOfRangeException("millisecondsTimeout");
            if (_id == 0) return true;
            JoinThreadTimeout(_id, millisecondsTimeout);
            return !JoinTimedOut();
        }

        public bool Join(TimeSpan timeout)
        {
            long millis = timeout.Ticks / TimeSpan.TicksPerMillisecond;
            if (millis < Timeout.Infinite || millis > 2147483647L) throw new ArgumentOutOfRangeException("timeout");
            return Join((int)millis);
        }

        public static Thread CurrentThread { get { return _current; } }

#if LAMELLA_SURFACE_NETFX_1_1
        public int ManagedThreadId { get { return _id == 0 ? 1 : _id + 1; } }
#endif

        public bool IsAlive { get { return true; } }

        public ThreadPriority Priority
        {
            get { return _priority; }
            set
            {
                if (value < ThreadPriority.Lowest || value > ThreadPriority.Highest)
                {
                    throw new ArgumentException("value");
                }
                _priority = value;
            }
        }


        public string Name
        {
            get { return _name; }
            set { _name = value; }
        }

        public static void Sleep(int millisecondsTimeout) { SleepThread(millisecondsTimeout); }

        public static void Sleep(TimeSpan timeout)
        {
            long millis = timeout.Ticks / TimeSpan.TicksPerMillisecond;
            if (millis < Timeout.Infinite || millis > 2147483647L) throw new ArgumentOutOfRangeException("timeout");
            Sleep((int)millis);
        }

        public static void SpinWait(int iterations)
        {
            if (iterations > 0) YieldThread();
        }

#if LAMELLA_SURFACE_NETFX_4_0
        public static bool Yield() { YieldThread(); return true; }
#endif

        private static void ThreadEntry(ThreadStart start) { start(); }

        [Lamella.Runtime.RuntimeProvided] private static int StartThread(ThreadStart start, bool isBackground) { return 0; }
        [Lamella.Runtime.RuntimeProvided] private static void JoinThread(int id) { }
        [Lamella.Runtime.RuntimeProvided] private static void JoinThreadTimeout(int id, int millisecondsTimeout) { }
        [Lamella.Runtime.RuntimeProvided] private static bool JoinTimedOut() { return false; }
        [Lamella.Runtime.RuntimeProvided] private static void YieldThread() { }
        [Lamella.Runtime.RuntimeProvided] private static void SleepThread(int millisecondsTimeout) { }
    }
}
#endif
