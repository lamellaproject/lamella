// Lamella managed corlib (from scratch). -- System.Collections.Generic.EqualityComparer<T>
#if LAMELLA_SURFACE_NETFX_2_0
namespace System.Collections.Generic
{
    /// <summary>Provides a default equality comparison for <typeparamref name="T"/>.</summary>
    /// <typeparam name="T">The type of objects to compare.</typeparam>
    public abstract class EqualityComparer<T>
    {
        private static EqualityComparer<T> _default;

        /// <summary>Initializes the comparer.</summary>
        protected EqualityComparer()
        {
        }

        /// <summary>A default comparer for <typeparamref name="T"/>.</summary>
        public static EqualityComparer<T> Default
        {
            get
            {
                if (_default == null)
                {
                    _default = new ObjectEqualityComparer<T>();
                }
                return _default;
            }
        }

        /// <summary>Whether two values are equal.</summary>
        /// <param name="x">The first value.</param>
        /// <param name="y">The second value.</param>
        /// <returns>True when the two are equal.</returns>
        public abstract bool Equals(T x, T y);

        /// <summary>A hash code for <paramref name="obj"/>.</summary>
        /// <param name="obj">The value to hash.</param>
        /// <returns>The hash code.</returns>
        public abstract int GetHashCode(T obj);
    }

    internal sealed class ObjectEqualityComparer<T> : EqualityComparer<T>
    {
        public override bool Equals(T x, T y)
        {
            if ((object)x != null)
            {
                if ((object)y != null)
                {
                    return x.Equals(y);
                }
                return false;
            }
            return (object)y == null;
        }

        public override int GetHashCode(T obj)
        {
            if ((object)obj == null)
            {
                return 0;
            }
            return obj.GetHashCode();
        }
    }
}
#endif
