// Lamella managed corlib (from scratch). -- System.Threading.Tasks.TaskCompletionSource<TResult>
#if LAMELLA_SURFACE_NETFX_4_0
namespace System.Threading.Tasks
{
    /// <summary>Produces a <see cref="System.Threading.Tasks.Task{TResult}"/> that code settles
    /// later.</summary>
    /// <typeparam name="TResult">The type of the task's value.</typeparam>
    public class TaskCompletionSource<TResult>
    {
        private readonly System.Threading.Tasks.Task<TResult> _task;

        /// <summary>A source with a pending task.</summary>
        public TaskCompletionSource()
        {
            _task = new System.Threading.Tasks.Task<TResult>();
        }

        /// <summary>The task this source settles.</summary>
        public System.Threading.Tasks.Task<TResult> Task
        {
            get { return _task; }
        }

        /// <summary>Completes the task with <paramref name="result"/>.</summary>
        /// <param name="result">The value.</param>
        /// <exception cref="InvalidOperationException">The task has already finished.</exception>
        public void SetResult(TResult result)
        {
            _task.SetResult(result);
        }

        /// <summary>Completes the task with <paramref name="result"/>, unless it has already
        /// finished.</summary>
        /// <param name="result">The value.</param>
        /// <returns>True when this call completed the task.</returns>
        public bool TrySetResult(TResult result)
        {
            return _task.TrySetResult(result);
        }

        /// <summary>Completes the task with <paramref name="exception"/>.</summary>
        /// <param name="exception">The exception.</param>
        /// <exception cref="ArgumentNullException"><paramref name="exception"/> is null.</exception>
        /// <exception cref="InvalidOperationException">The task has already finished.</exception>
        public void SetException(Exception exception)
        {
            if (exception == null) throw new ArgumentNullException("exception");
            _task.SetException(exception);
        }

        /// <summary>Completes the task with <paramref name="exception"/>, unless it has already
        /// finished.</summary>
        /// <param name="exception">The exception.</param>
        /// <returns>True when this call completed the task.</returns>
        /// <exception cref="ArgumentNullException"><paramref name="exception"/> is null.</exception>
        public bool TrySetException(Exception exception)
        {
            if (exception == null) throw new ArgumentNullException("exception");
            return _task.TrySettle(exception);
        }
    }
}
#endif
