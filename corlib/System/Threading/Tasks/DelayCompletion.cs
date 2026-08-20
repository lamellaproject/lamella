// Lamella managed corlib (from scratch). -- System.Threading.Tasks.DelayCompletion
#if LAMELLA_SURFACE_NETFX_4_5 && LAMELLA_SURFACE_THREADS
namespace System.Threading.Tasks
{
    internal sealed class DelayCompletion
    {
        private Task _task;
        private Timer _timer;

        internal DelayCompletion(Task task, int millisecondsDelay)
        {
            _task = task;
            _timer = new Timer(new TimerCallback(Fire), null, millisecondsDelay, -1);
        }

        private void Fire(object state)
        {
            Timer timer = _timer;
            _timer = null;
            Task task = _task;
            _task = null;
            if (timer != null) timer.Dispose();
            if (task != null) task.SetResult();
        }
    }
}
#endif
