// Lamella managed corlib (from scratch). -- System.Collections.Generic.IEnumerable<T>
#if LAMELLA_SURFACE_NETFX_2_0
namespace System.Collections.Generic
{

    /// A sequence of `T` that exposes an enumerator.
    public interface IEnumerable<T> : IEnumerable
    {
        /// An enumerator positioned before the first element.
        new IEnumerator<T> GetEnumerator();
    }
}
#endif
