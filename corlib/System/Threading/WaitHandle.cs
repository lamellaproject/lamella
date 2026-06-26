// Lamella managed corlib (from scratch). -- System.Threading.WaitHandle
#if LAMELLA_SURFACE_WAIT_HANDLES
namespace System.Threading
{
    public abstract class WaitHandle : IDisposable
    {
        public const int WaitTimeout = 258;

        private static readonly object coordinator = new object();
        private bool signaled;
        private readonly bool autoReset;

        internal WaitHandle(bool initialState, bool autoReset)
        {
            signaled = initialState;
            this.autoReset = autoReset;
        }

        public virtual bool WaitOne()
        {
            lock (coordinator)
            {
                while (!signaled)
                {
                    Monitor.Wait(coordinator);
                }
                if (autoReset)
                {
                    signaled = false;
                }
                return true;
            }
        }

        public virtual bool WaitOne(int millisecondsTimeout, bool exitContext)
        {
            return WaitOne();
        }

        public virtual void Close()
        {
        }

        public void Dispose()
        {
            Close();
        }

        public static bool WaitAll(WaitHandle[] waitHandles)
        {
            lock (coordinator)
            {
                while (true)
                {
                    bool all = true;
                    for (int i = 0; i < waitHandles.Length; i++)
                    {
                        if (!waitHandles[i].signaled)
                        {
                            all = false;
                            break;
                        }
                    }
                    if (all)
                    {
                        for (int i = 0; i < waitHandles.Length; i++)
                        {
                            if (waitHandles[i].autoReset)
                            {
                                waitHandles[i].signaled = false;
                            }
                        }
                        return true;
                    }
                    Monitor.Wait(coordinator);
                }
            }
        }

        public static bool WaitAll(WaitHandle[] waitHandles, int millisecondsTimeout, bool exitContext)
        {
            return WaitAll(waitHandles);
        }

        public static int WaitAny(WaitHandle[] waitHandles)
        {
            lock (coordinator)
            {
                while (true)
                {
                    for (int i = 0; i < waitHandles.Length; i++)
                    {
                        if (waitHandles[i].signaled)
                        {
                            if (waitHandles[i].autoReset)
                            {
                                waitHandles[i].signaled = false;
                            }
                            return i;
                        }
                    }
                    Monitor.Wait(coordinator);
                }
            }
        }

        public static int WaitAny(WaitHandle[] waitHandles, int millisecondsTimeout, bool exitContext)
        {
            return WaitAny(waitHandles);
        }

        internal void SetSignal()
        {
            lock (coordinator)
            {
                signaled = true;
                Monitor.PulseAll(coordinator);
            }
        }

        internal void ResetSignal()
        {
            lock (coordinator)
            {
                signaled = false;
            }
        }
    }
}
#endif
