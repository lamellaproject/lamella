// Lamella managed corlib (from scratch). -- System.Collections.Generic.DefaultEquality<T>
#if LAMELLA_SURFACE_NETFX_2_0
namespace System.Collections.Generic
{
    internal sealed class DefaultEquality<T>
    {
        private DefaultEquality()
        {
        }

        internal static bool AreEqual(T x, T y)
        {
            object left = x;
            object right = y;
            if (left == null) return right == null;
            if (right == null) return false;
            IEquatable<T> equatable = left as IEquatable<T>;
            if (equatable != null) return equatable.Equals(y);
            return left.Equals(right);
        }
    }
}
#endif
