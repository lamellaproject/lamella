// Lamella managed corlib (from scratch). -- System.Delegate / System.MulticastDelegate
namespace System
{
    public abstract class Delegate
    {
        private object _target;

        private IntPtr _methodPtr;
    }

    public abstract class MulticastDelegate : Delegate
    {
        private Delegate[] _invocationList;
    }
}
