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

        public static Thread CurrentThread { get { return _current; } }

#if LAMELLA_SURFACE_NETFX_1_1
        public int ManagedThreadId { get { return _id == 0 ? 1 : _id + 1; } }
#endif

        public bool IsAlive { get { return true; } }

        public string Name
        {
            get { return _name; }
            set { _name = value; }
        }

        public static void Sleep(int millisecondsTimeout) { SleepThread(millisecondsTimeout); }

#if LAMELLA_SURFACE_NETFX_4_0
        public static bool Yield() { YieldThread(); return true; }
#endif

        private static void ThreadEntry(ThreadStart start) { start(); }

        [Lamella.Runtime.RuntimeProvided] private static int StartThread(ThreadStart start, bool isBackground) { return 0; }
        [Lamella.Runtime.RuntimeProvided] private static void JoinThread(int id) { }
        [Lamella.Runtime.RuntimeProvided] private static void YieldThread() { }
        [Lamella.Runtime.RuntimeProvided] private static void SleepThread(int millisecondsTimeout) { }
    }
}
#endif
