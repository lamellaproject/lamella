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

        public System.Runtime.CompilerServices.TaskAwaiter GetAwaiter()
        {
            return new System.Runtime.CompilerServices.TaskAwaiter(this);
        }

        internal void SetResult() { Settle(null); }

        internal void SetException(Exception exception) { Settle(exception); }

        private void Settle(Exception exception)
        {
            if (_completed) throw new InvalidOperationException("The task is already completed.");
            _exception = exception;
            _completed = true;
            Action first = _continuation;
            Action[] rest = _more;
            int restCount = _moreCount;
            _continuation = null;
            _more = null;
            _moreCount = 0;
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
            if (_continuation == null) { _continuation = continuation; return; }
            if (_more == null) _more = new Action[2];
            if (_moreCount == _more.Length)
            {
                Action[] grown = new Action[_more.Length * 2];
                for (int i = 0; i < _moreCount; i++) grown[i] = _more[i];
                _more = grown;
            }
            _more[_moreCount] = continuation;
            _moreCount = _moreCount + 1;
        }

        internal void ThrowIfFaulted()
        {
            if (_exception != null) throw _exception;
        }
    }
}
#endif
