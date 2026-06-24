// Lamella managed corlib (from scratch). -- System.Threading.Thread
namespace System.Threading
{
    public sealed class Thread
    {
        private static Thread _current = new Thread();
        private string _name;

        internal Thread() { }

        public static Thread CurrentThread { get { return _current; } }

        public int ManagedThreadId { get { return 1; } }

        public bool IsAlive { get { return true; } }

        public string Name
        {
            get { return _name; }
            set { _name = value; }
        }

        public static void Sleep(int millisecondsTimeout) { }
    }
}
