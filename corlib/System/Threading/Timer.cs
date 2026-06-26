// Lamella managed corlib (from scratch). -- System.Threading.Timer
#if LAMELLA_SURFACE_THREADS
namespace System.Threading
{
    public sealed class Timer : IDisposable
    {
        private TimerCallback _callback;
        private object _state;
        private int _dueTime;
        private int _period;
        private bool _disposed;

        public Timer(TimerCallback callback, object state, int dueTime, int period)
        {
            _callback = callback;
            _state = state;
            _dueTime = dueTime;
            _period = period;
            Thread thread = new Thread(new ThreadStart(RunLoop));
            thread.IsBackground = true;
            thread.Start();
        }

        public Timer(TimerCallback callback, object state, TimeSpan dueTime, TimeSpan period)
            : this(callback, state, (int)dueTime.TotalMilliseconds, (int)period.TotalMilliseconds)
        {
        }

        private void RunLoop()
        {
            if (_dueTime < 0)
            {
                return;
            }
            Thread.Sleep(_dueTime);
            while (!_disposed)
            {
                _callback(_state);
                if (_period <= 0)
                {
                    return;
                }
                Thread.Sleep(_period);
            }
        }

        public bool Change(int dueTime, int period)
        {
            _dueTime = dueTime;
            _period = period;
            return true;
        }

        public bool Change(TimeSpan dueTime, TimeSpan period)
        {
            return Change((int)dueTime.TotalMilliseconds, (int)period.TotalMilliseconds);
        }

        public void Dispose()
        {
            _disposed = true;
        }
    }
}
#endif
