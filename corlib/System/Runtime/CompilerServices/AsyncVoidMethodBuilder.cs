// Lamella managed corlib (from scratch). -- System.Runtime.CompilerServices.AsyncVoidMethodBuilder
#if LAMELLA_SURFACE_NETFX_4_5
namespace System.Runtime.CompilerServices
{
    public struct AsyncVoidMethodBuilder
    {
        public static AsyncVoidMethodBuilder Create()
        {
            return new AsyncVoidMethodBuilder();
        }

        public void SetResult()
        {
        }

        public void SetException(Exception exception)
        {
            throw exception;
        }
    }
}
#endif
