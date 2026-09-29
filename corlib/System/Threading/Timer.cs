// Lamella managed corlib (from scratch). -- System.Threading.Timer
#if LAMELLA_SURFACE_THREADS
namespace System.Threading
{
    public sealed class Timer : IDisposable
    {
        private const int SliceMilliseconds = 50;

        private TimerCallback _callback;
        private object _state;
        private object _lock;
        private int _dueTime;
        private int _period;
        private int _changes;
        private bool _disposed;

        public Timer(TimerCallback callback, object state, int dueTime, int period)
        {
            if (callback == null) throw new ArgumentNullException("callback");
            CheckTimes(dueTime, period);
            _callback = callback;
            _state = state;
            _lock = new object();
            _dueTime = dueTime;
            _period = period;
            Thread thread = new Thread(new ThreadStart(RunLoop));
            thread.IsBackground = true;
            thread.Start();
        }

        public Timer(TimerCallback callback, object state, TimeSpan dueTime, TimeSpan period)
            : this(
                callback,
                state,
                (int)(dueTime.Ticks / TimeSpan.TicksPerMillisecond),
                (int)(period.Ticks / TimeSpan.TicksPerMillisecond))
        {
        }

        private static void CheckTimes(int dueTime, int period)
        {
            if (dueTime < Timeout.Infinite) throw new ArgumentOutOfRangeException("dueTime");
            if (period < Timeout.Infinite) throw new ArgumentOutOfRangeException("period");
        }

        private void RunLoop()
        {
            Monitor.Enter(_lock);
            try
            {
                while (!_disposed)
                {
                    if (_dueTime == Timeout.Infinite)
                    {
                        Monitor.Wait(_lock);
                        continue;
                    }
                    if (!WaitDue()) continue;
                    _dueTime = (_period == Timeout.Infinite || _period == 0) ? Timeout.Infinite : _period;
                    Monitor.Exit(_lock);
                    try
                    {
                        _callback(_state);
                    }
                    finally
                    {
                        Monitor.Enter(_lock);
                    }
                }
            }
            finally
            {
                Monitor.Exit(_lock);
            }
        }

        private bool WaitDue()
        {
            int changes = _changes;
            int due = _dueTime;
            int start = Environment.TickCount;
            int slept = 0;
            while (true)
            {
                int byClock = unchecked(Environment.TickCount - start);
                int elapsed = byClock > slept ? byClock : slept;
                if (elapsed >= due) return true;
                int slice = due - elapsed;
                if (slice > SliceMilliseconds) slice = SliceMilliseconds;
                Monitor.Exit(_lock);
                try
                {
                    Thread.Sleep(slice);
                }
                finally
                {
                    Monitor.Enter(_lock);
                }
                slept = slept + slice;
                if (_disposed || _changes != changes) return false;
            }
        }

        public bool Change(int dueTime, int period)
        {
            CheckTimes(dueTime, period);
            Monitor.Enter(_lock);
            try
            {
                _dueTime = dueTime;
                _period = period;
                _changes = _changes + 1;
                Monitor.Pulse(_lock);
            }
            finally
            {
                Monitor.Exit(_lock);
            }
            return true;
        }

        public bool Change(TimeSpan dueTime, TimeSpan period)
        {
            return Change(
                (int)(dueTime.Ticks / TimeSpan.TicksPerMillisecond),
                (int)(period.Ticks / TimeSpan.TicksPerMillisecond));
        }

        public void Dispose()
        {
            Monitor.Enter(_lock);
            try
            {
                _disposed = true;
                Monitor.Pulse(_lock);
            }
            finally
            {
                Monitor.Exit(_lock);
            }
        }
    }
}
#endif
