// Lamella managed corlib (from scratch). -- System.Runtime.CompilerServices.AsyncTaskMethodBuilder
#if LAMELLA_SURFACE_NETFX_4_0
namespace System.Runtime.CompilerServices
{
    public struct AsyncTaskMethodBuilder
    {
        private System.Threading.Tasks.Task _task;

        public static AsyncTaskMethodBuilder Create()
        {
            AsyncTaskMethodBuilder builder = new AsyncTaskMethodBuilder();
            builder._task = new System.Threading.Tasks.Task();
            return builder;
        }

        public System.Threading.Tasks.Task Task
        {
            get { return _task; }
        }

        public void SetResult()
        {
            _task.SetResult();
        }

        public void SetException(Exception exception)
        {
            _task.SetException(exception);
        }
    }
}
#endif
