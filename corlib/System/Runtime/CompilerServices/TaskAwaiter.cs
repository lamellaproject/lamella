// Lamella managed corlib (from scratch). -- System.Runtime.CompilerServices.TaskAwaiter
#if LAMELLA_SURFACE_NETFX_4_0
namespace System.Runtime.CompilerServices
{
    public struct TaskAwaiter : ICriticalNotifyCompletion
    {
        private readonly System.Threading.Tasks.Task _task;

        internal TaskAwaiter(System.Threading.Tasks.Task task) { _task = task; }

        public bool IsCompleted { get { return _task == null || _task.IsCompleted; } }

        public void GetResult()
        {
            if (_task != null) _task.ThrowIfFaulted();
        }

        public void OnCompleted(System.Action continuation)
        {
            if (_task == null) { if (continuation != null) continuation(); return; }
            _task.AddContinuation(continuation);
        }

        public void UnsafeOnCompleted(System.Action continuation)
        {
            OnCompleted(continuation);
        }
    }
}
#endif
