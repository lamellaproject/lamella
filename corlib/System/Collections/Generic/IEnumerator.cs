// Lamella managed corlib (from scratch). -- System.Collections.Generic.IEnumerator<T>
#if LAMELLA_SURFACE_NETFX_2_0
namespace System.Collections.Generic
{

    /// An enumerator over a sequence of `T`.
    public interface IEnumerator<T> : IEnumerator, IDisposable
    {
        /// The element at the cursor.
        new T Current { get; }
    }
}
#endif
