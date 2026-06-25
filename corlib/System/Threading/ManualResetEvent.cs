// Lamella managed corlib (from scratch). -- System.Threading.ManualResetEvent
namespace System.Threading
{
    public sealed class ManualResetEvent : WaitHandle
    {
        public ManualResetEvent(bool initialState) : base(initialState, false)
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
