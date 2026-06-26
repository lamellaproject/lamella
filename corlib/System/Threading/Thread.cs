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

        public void Start() { _id = StartThread(_start, _isBackground); }

        public bool IsBackground
        {
            get { return _isBackground; }
            set { _isBackground = value; }
        }

        public void Join() { if (_id != 0) JoinThread(_id); }

        public static Thread CurrentThread { get { return _current; } }

        public int ManagedThreadId { get { return _id == 0 ? 1 : _id + 1; } }

        public bool IsAlive { get { return true; } }

        public string Name
        {
            get { return _name; }
            set { _name = value; }
        }

        public static void Sleep(int millisecondsTimeout) { SleepThread(millisecondsTimeout); }

        public static bool Yield() { YieldThread(); return true; }

        [Lamella.Runtime.RuntimeProvided] private static int StartThread(ThreadStart start, bool isBackground) { return 0; }
        [Lamella.Runtime.RuntimeProvided] private static void JoinThread(int id) { }
        [Lamella.Runtime.RuntimeProvided] private static void YieldThread() { }
        [Lamella.Runtime.RuntimeProvided] private static void SleepThread(int millisecondsTimeout) { }
    }
}
#endif
