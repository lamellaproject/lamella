// Lamella managed corlib (from scratch). -- System.IComparable and System.IComparable<T>
namespace System
{
    /// <summary>Defines a sort order for the instances of a type, against any object.</summary>
    public interface IComparable
    {
        int CompareTo(object obj);
    }

#if LAMELLA_SURFACE_NETFX_2_0
    /// <summary>Defines a sort order for the instances of a type, against another value of a given
    /// type, so a value can be compared without boxing.</summary>
    /// <typeparam name="T">The type of value this instance compares against.</typeparam>
    public interface IComparable<T>
    {
        /// <summary>Compares this instance with another value.</summary>
        /// <param name="other">The value to compare against.</param>
        /// <returns>Less than zero when this instance sorts before <paramref name="other"/>, zero when
        /// the two sort together, and greater than zero when it sorts after.</returns>
        int CompareTo(T other);
    }
#endif
}
