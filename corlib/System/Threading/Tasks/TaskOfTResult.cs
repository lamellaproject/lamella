// Lamella managed corlib (from scratch). -- System.Threading.Tasks.Task<TResult>
#if LAMELLA_SURFACE_NETFX_4_0
namespace System.Threading.Tasks
{
    /// <summary>An operation that produces a value of type <typeparamref name="TResult"/> when it
    /// finishes.</summary>
    /// <typeparam name="TResult">The type of the value.</typeparam>
    public class Task<TResult> : Task
    {
        private TResult _result;

        internal Task() { }

        /// <summary>The value the task produced, once it has finished; reading it waits for a task that
        /// has not.</summary>
        /// <exception cref="AggregateException">The task faulted; its exception is the inner
        /// exception.</exception>
        public TResult Result
        {
            get
            {
                if (!IsCompleted) ParkUntilSettled();
                ThrowIfFaultedAggregate();
                return _result;
            }
        }

#if LAMELLA_SURFACE_NETFX_4_5
        /// <summary>The awaiter an <c>await</c> of this task uses.</summary>
        /// <returns>The awaiter.</returns>
        public new System.Runtime.CompilerServices.TaskAwaiter<TResult> GetAwaiter()
        {
            return new System.Runtime.CompilerServices.TaskAwaiter<TResult>(this);
        }
#endif

        internal TResult ResultOnSuccess
        {
            get { return _result; }
        }

        internal void SetResult(TResult result)
        {
            if (!TrySetResult(result)) throw new InvalidOperationException(AlreadySettled);
        }

        internal bool TrySetResult(TResult result)
        {
            Action first;
            Action[] rest;
            int restCount;
            bool settled;
#if LAMELLA_SURFACE_THREADS
            lock (this)
            {
                settled = StoreAndTransitionLocked(result, out first, out rest, out restCount);
            }
#else
            settled = StoreAndTransitionLocked(result, out first, out rest, out restCount);
#endif
            if (settled) RunContinuations(first, rest, restCount);
            return settled;
        }

        private bool StoreAndTransitionLocked(TResult result, out Action first, out Action[] rest, out int restCount)
        {
            if (IsCompleted)
            {
                first = null;
                rest = null;
                restCount = 0;
                return false;
            }
            _result = result;
            return TransitionLocked(null, out first, out rest, out restCount);
        }
    }
}
#endif
