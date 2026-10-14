// Lamella managed corlib (from scratch). -- System.Runtime.CompilerServices.TaskAwaiter<TResult>
#if LAMELLA_SURFACE_NETFX_4_5
namespace System.Runtime.CompilerServices
{
    /// <summary>Waits for a <see cref="System.Threading.Tasks.Task{TResult}"/> to finish and hands back
    /// its value; what an <c>await</c> of the task calls.</summary>
    /// <typeparam name="TResult">The type of the task's value.</typeparam>
    public struct TaskAwaiter<TResult> : ICriticalNotifyCompletion
    {
        private readonly System.Threading.Tasks.Task<TResult> _task;

        internal TaskAwaiter(System.Threading.Tasks.Task<TResult> task) { _task = task; }

        /// <summary>Whether the task has finished, so an <c>await</c> need not suspend.</summary>
        public bool IsCompleted { get { return _task.IsCompleted; } }

        /// <summary>The task's value, waiting for a task that has not finished.</summary>
        /// <returns>The value.</returns>
        public TResult GetResult()
        {
            if (!_task.IsCompleted) _task.ParkUntilSettled();
            _task.ThrowIfFaulted();
            return _task.ResultOnSuccess;
        }

        /// <summary>Runs <paramref name="continuation"/> when the task finishes.</summary>
        /// <param name="continuation">What to run.</param>
        /// <exception cref="ArgumentNullException"><paramref name="continuation"/> is null.</exception>
        public void OnCompleted(Action continuation)
        {
            if (continuation == null) throw new ArgumentNullException("continuation");
            _task.AddContinuation(continuation);
        }

        /// <summary>Runs <paramref name="continuation"/> when the task finishes.</summary>
        /// <param name="continuation">What to run.</param>
        /// <exception cref="ArgumentNullException"><paramref name="continuation"/> is null.</exception>
        public void UnsafeOnCompleted(Action continuation)
        {
            OnCompleted(continuation);
        }
    }
}
#endif
