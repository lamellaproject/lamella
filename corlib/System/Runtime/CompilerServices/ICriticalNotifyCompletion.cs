// Lamella managed corlib (from scratch). -- System.Runtime.CompilerServices.ICriticalNotifyCompletion
#if LAMELLA_SURFACE_NETFX_4_5
namespace System.Runtime.CompilerServices
{
    public interface ICriticalNotifyCompletion : INotifyCompletion
    {
        void UnsafeOnCompleted(System.Action continuation);
    }
}
#endif
