// Lamella managed corlib (from scratch). -- System.Runtime.CompilerServices.INotifyCompletion
#if LAMELLA_SURFACE_NETFX_4_0
namespace System.Runtime.CompilerServices
{
    public interface INotifyCompletion
    {
        void OnCompleted(System.Action continuation);
    }
}
#endif
