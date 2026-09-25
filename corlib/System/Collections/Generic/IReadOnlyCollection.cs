// Lamella managed corlib (from scratch). -- System.Collections.Generic.IReadOnlyCollection<T>
#if LAMELLA_SURFACE_NETFX_4_5
namespace System.Collections.Generic
{

    /// <summary>A sequence of <typeparamref name="T"/> whose element count is known.</summary>
    /// <typeparam name="T">The type of the elements.</typeparam>
    public interface IReadOnlyCollection<T> : IEnumerable<T>
    {
        /// <summary>How many elements the collection holds.</summary>
        int Count { get; }
    }
}
#endif
