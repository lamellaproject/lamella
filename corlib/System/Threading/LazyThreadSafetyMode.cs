// Lamella managed corlib (from scratch). -- System.Threading.LazyThreadSafetyMode
#if LAMELLA_SURFACE_NETFX_4_0
namespace System.Threading
{
    /// <summary>How a <see cref="System.Lazy{T}"/> makes its value when several threads may ask for it.</summary>
    public enum LazyThreadSafetyMode
    {
        /// <summary>No thread safety: the instance must not be used from more than one thread.</summary>
        None = 0,

        /// <summary>Threads may race to make the value, and the first one made is the one kept.</summary>
        PublicationOnly = 1,

        /// <summary>Only one thread makes the value, under a lock.</summary>
        ExecutionAndPublication = 2,
    }
}
#endif
