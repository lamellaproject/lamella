// Lamella managed corlib (from scratch). -- System.Runtime.CompilerServices.IAsyncStateMachine
#if LAMELLA_SURFACE_NETFX_4_0
namespace System.Runtime.CompilerServices
{
    public interface IAsyncStateMachine
    {
        void MoveNext();

        void SetStateMachine(IAsyncStateMachine stateMachine);
    }
}
#endif
