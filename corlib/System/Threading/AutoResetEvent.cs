// Lamella managed corlib (from scratch). -- System.Threading.AutoResetEvent
namespace System.Threading
{
    public sealed class AutoResetEvent : WaitHandle
    {
        public AutoResetEvent(bool initialState) : base(initialState, true)
        {
        }

        public bool Set()
        {
            SetSignal();
            return true;
        }

        public bool Reset()
        {
            ResetSignal();
            return true;
        }
    }
}
