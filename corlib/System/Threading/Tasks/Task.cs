// Lamella managed corlib (from scratch). -- System.Threading.Tasks.Task
#if LAMELLA_SURFACE_NETFX_4_0
namespace System.Threading.Tasks
{
    public class Task
    {
        private bool _completed;
        private Exception _exception;
        private Action _continuation;
        private Action[] _more;
        private int _moreCount;

        private static Task _completedTask;

        internal Task() { }

        public bool IsCompleted { get { return _completed; } }

        public bool IsFaulted { get { return _completed && _exception != null; } }

        public bool IsCompletedSuccessfully { get { return _completed && _exception == null; } }

#if LAMELLA_SURFACE_NETFX_4_5
        public static Task CompletedTask
        {
            get
            {
                if (_completedTask == null)
                {
                    Task task = new Task();
                    task.Settle(null);
                    _completedTask = task;
                }
                return _completedTask;
            }
        }
#endif

#if LAMELLA_SURFACE_NETFX_4_5
        public System.Runtime.CompilerServices.TaskAwaiter GetAwaiter()
        {
            return new System.Runtime.CompilerServices.TaskAwaiter(this);
        }

#if LAMELLA_SURFACE_THREADS
        public static Task Delay(int millisecondsDelay)
        {
            if (millisecondsDelay < -1) throw new ArgumentOutOfRangeException("millisecondsDelay");
            if (millisecondsDelay == 0) return CompletedTask;
            Task task = new Task();
            if (millisecondsDelay == Timeout.Infinite) return task;
            new DelayCompletion(task, millisecondsDelay);
            return task;
        }
#endif
#endif

#if LAMELLA_SURFACE_THREADS
        public void Wait()
        {
            Wait(Timeout.Infinite);
        }

        public bool Wait(int millisecondsTimeout)
        {
            if (millisecondsTimeout < -1) throw new ArgumentOutOfRangeException("millisecondsTimeout");
            if (!ParkFor(millisecondsTimeout)) return false;
            ThrowIfFaulted();
            return true;
        }

        internal void ParkUntilSettled()
        {
            if (_completed) return;
            lock (this)
            {
                while (!_completed) Monitor.Wait(this);
            }
        }

        private bool ParkFor(int millisecondsTimeout)
        {
            if (millisecondsTimeout < 0)
            {
                ParkUntilSettled();
                return true;
            }
            if (_completed) return true;
            lock (this)
            {
                int start = Environment.TickCount;
                while (!_completed)
                {
                    int elapsed = unchecked(Environment.TickCount - start);
                    if (elapsed >= millisecondsTimeout) return false;
                    if (!Monitor.Wait(this, millisecondsTimeout - elapsed)) return false;
                }
            }
            return true;
        }
#else
        internal void ParkUntilSettled()
        {
            if (!_completed) throw new PlatformNotSupportedException();
        }
#endif

        internal void SetResult() { Settle(null); }

        internal void SetException(Exception exception) { Settle(exception); }

        internal bool TrySettle(Exception exception)
        {
            return Complete(exception, false);
        }

        private void Settle(Exception exception)
        {
            Complete(exception, true);
        }

        internal const string AlreadySettled = "An attempt was made to transition a task to a final state when it had already completed.";

        private bool Complete(Exception exception, bool refuseIfSettled)
        {
            Action first;
            Action[] rest;
            int restCount;
            bool settled;
#if LAMELLA_SURFACE_THREADS
            lock (this)
            {
                settled = TransitionLocked(exception, out first, out rest, out restCount);
            }
#else
            settled = TransitionLocked(exception, out first, out rest, out restCount);
#endif
            if (!settled)
            {
                if (refuseIfSettled) throw new InvalidOperationException(AlreadySettled);
                return false;
            }
            RunContinuations(first, rest, restCount);
            return true;
        }

        internal bool TransitionLocked(Exception exception, out Action first, out Action[] rest, out int restCount)
        {
            first = null;
            rest = null;
            restCount = 0;
            if (_completed) return false;
            _exception = exception;
            _completed = true;
#if LAMELLA_SURFACE_THREADS
            Monitor.PulseAll(this);
#endif
            first = _continuation;
            rest = _more;
            restCount = _moreCount;
            _continuation = null;
            _more = null;
            _moreCount = 0;
            return true;
        }

        internal static void RunContinuations(Action first, Action[] rest, int restCount)
        {
            if (first != null) first();
            for (int i = 0; i < restCount; i++)
            {
                if (rest[i] != null) rest[i]();
            }
        }

        internal void AddContinuation(Action continuation)
        {
            if (continuation == null) return;
            if (_completed) { continuation(); return; }
            bool stored;
#if LAMELLA_SURFACE_THREADS
            lock (this)
            {
                stored = TryStoreLocked(continuation);
            }
#else
            stored = TryStoreLocked(continuation);
#endif
            if (!stored) continuation();
        }

        private bool TryStoreLocked(Action continuation)
        {
            if (_completed) return false;
            if (_continuation == null) { _continuation = continuation; return true; }
            if (_more == null) _more = new Action[2];
            if (_moreCount == _more.Length)
            {
                Action[] grown = new Action[_more.Length * 2];
                for (int i = 0; i < _moreCount; i++) grown[i] = _more[i];
                _more = grown;
            }
            _more[_moreCount] = continuation;
            _moreCount = _moreCount + 1;
            return true;
        }

        internal void ThrowIfFaulted()
        {
            if (_exception != null) throw _exception;
        }

        internal void ThrowIfFaultedAggregate()
        {
            if (_exception != null) throw new AggregateException(_exception);
        }
    }
}
#endif
