// Lamella managed corlib (from scratch). -- System.Runtime.CompilerServices.AsyncTaskMethodBuilder<TResult>
#if LAMELLA_SURFACE_NETFX_4_5
namespace System.Runtime.CompilerServices
{
    /// <summary>Builds the task an <c>async</c> method returning
    /// <see cref="System.Threading.Tasks.Task{TResult}"/> hands back; compiler-generated code uses it.</summary>
    /// <typeparam name="TResult">The type of the method's value.</typeparam>
    public struct AsyncTaskMethodBuilder<TResult>
    {
        private System.Threading.Tasks.Task<TResult> _task;

        /// <summary>A builder with a pending task.</summary>
        /// <returns>The builder.</returns>
        public static AsyncTaskMethodBuilder<TResult> Create()
        {
            AsyncTaskMethodBuilder<TResult> builder = new AsyncTaskMethodBuilder<TResult>();
            builder._task = new System.Threading.Tasks.Task<TResult>();
            return builder;
        }

        /// <summary>The task the method returns, pending until the method finishes.</summary>
        public System.Threading.Tasks.Task<TResult> Task
        {
            get { return _task; }
        }

        /// <summary>Completes the task with <paramref name="result"/>.</summary>
        /// <param name="result">The method's value.</param>
        /// <exception cref="InvalidOperationException">The task has already finished.</exception>
        public void SetResult(TResult result)
        {
            _task.SetResult(result);
        }

        /// <summary>Completes the task with <paramref name="exception"/>.</summary>
        /// <param name="exception">The exception the method threw.</param>
        /// <exception cref="ArgumentNullException"><paramref name="exception"/> is null.</exception>
        /// <exception cref="InvalidOperationException">The task has already finished.</exception>
        public void SetException(Exception exception)
        {
            if (exception == null) throw new ArgumentNullException("exception");
            _task.SetException(exception);
        }
    }
}
#endif
