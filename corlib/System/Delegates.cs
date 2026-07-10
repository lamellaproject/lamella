// Lamella managed corlib (from scratch). -- System.Delegate / System.MulticastDelegate
namespace System
{
    public abstract class Delegate
    {
        private object _target;

        private IntPtr _methodPtr;

        [Lamella.Runtime.RuntimeProvided] public static Delegate Combine(Delegate a, Delegate b) { return null; }

        [Lamella.Runtime.RuntimeProvided] public static Delegate Remove(Delegate source, Delegate value) { return null; }
    }

    public abstract class MulticastDelegate : Delegate
    {
        private Delegate[] _invocationList;
    }
}
