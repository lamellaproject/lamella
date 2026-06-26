// Lamella managed corlib (from scratch). -- System.ArgIterator + System.RuntimeArgumentHandle
#if LAMELLA_SURFACE_VARARGS
namespace System
{
    public struct RuntimeArgumentHandle
    {
    }

    public struct ArgIterator
    {
        private object _handle;
        private int _index;

        public ArgIterator(RuntimeArgumentHandle arglist)
        {
            _handle = ArgIteratorNative.Cookie(arglist);
            _index = 0;
        }

        public int GetRemainingCount()
        {
            return ArgIteratorNative.RemainingCount(_handle, _index);
        }

        public object GetNextArg()
        {
            object next = ArgIteratorNative.GetArg(_handle, _index);
            _index = _index + 1;
            return next;
        }
    }

    internal sealed class ArgIteratorNative
    {
        [Lamella.Runtime.RuntimeProvided] internal static object Cookie(RuntimeArgumentHandle handle) { return null; }
        [Lamella.Runtime.RuntimeProvided] internal static int RemainingCount(object handle, int index) { return 0; }
        [Lamella.Runtime.RuntimeProvided] internal static object GetArg(object handle, int index) { return null; }
    }
}
#endif
