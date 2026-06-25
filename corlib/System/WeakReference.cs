// Lamella managed corlib (from scratch). -- System.WeakReference
namespace System
{
    public class WeakReference
    {
        private object _cell;

        public WeakReference(object target)
        {
            _cell = MakeWeakCell(target);
        }

        public WeakReference(object target, bool trackResurrection)
        {
            _cell = MakeWeakCell(target);
        }

        public virtual object Target
        {
            get { return ReadWeakCell(_cell); }
            set { WriteWeakCell(_cell, value); }
        }

        public virtual bool IsAlive
        {
            get { return ReadWeakCell(_cell) != null; }
        }

        [Lamella.Runtime.RuntimeProvided] private static object MakeWeakCell(object target) { return null; }
        [Lamella.Runtime.RuntimeProvided] private static object ReadWeakCell(object cell) { return null; }
        [Lamella.Runtime.RuntimeProvided] private static void WriteWeakCell(object cell, object target) { }
    }
}
