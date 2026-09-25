// Lamella managed corlib (from scratch). -- System.SZArrayHelper<T>
#if LAMELLA_SURFACE_NETFX_2_0
namespace System
{
    internal sealed class SZArrayHelper<T>
    {
        private SZArrayHelper()
        {
        }

        private static T[] Vector(T[] array)
        {
            if (array.Rank != 1)
            {
                throw new InvalidCastException();
            }
            return array;
        }

        internal static int get_Count(T[] array)
        {
            return Vector(array).Length;
        }

        internal static bool get_IsReadOnly(T[] array)
        {
            return true;
        }

        internal static void Add(T[] array, T item)
        {
            throw new NotSupportedException("Collection was of a fixed size.");
        }

        internal static void Clear(T[] array)
        {
            throw new NotSupportedException("Collection is read-only.");
        }

        internal static bool Contains(T[] array, T item)
        {
            T[] vector = Vector(array);
            System.Collections.Generic.EqualityComparer<T> comparer = System.Collections.Generic.EqualityComparer<T>.Default;
            for (int i = 0; i < vector.Length; i++)
            {
                if (comparer.Equals(vector[i], item))
                {
                    return true;
                }
            }
            return false;
        }

        internal static void CopyTo(T[] array, T[] destination, int arrayIndex)
        {
            T[] vector = Vector(array);
            Array.Copy(vector, 0, destination, arrayIndex, vector.Length);
        }

        internal static bool Remove(T[] array, T item)
        {
            throw new NotSupportedException("Collection was of a fixed size.");
        }
    }
}
#endif
