// Lamella managed corlib (from scratch). -- System.Collections.Generic.Comparer<T>
#if LAMELLA_SURFACE_NETFX_2_0
namespace System.Collections.Generic
{
    /// <summary>Provides a base class for implementations of <see cref="IComparer{T}"/>, and the
    /// default order for <typeparamref name="T"/>.</summary>
    /// <typeparam name="T">The type of the values compared.</typeparam>
    public abstract class Comparer<T> : IComparer<T>, IComparer
    {
        private static Comparer<T> _default;

        /// <summary>Initializes the comparer.</summary>
        protected Comparer()
        {
        }

        /// <summary>The default comparer for <typeparamref name="T"/>: a null value sorts before any
        /// other, and two values are otherwise ordered by <see cref="IComparable{T}"/> or
        /// <see cref="IComparable"/>.</summary>
        public static Comparer<T> Default
        {
            get
            {
                if (_default == null)
                {
                    _default = new ObjectComparer<T>();
                }
                return _default;
            }
        }

#if LAMELLA_SURFACE_NETFX_4_5
        /// <summary>A comparer that orders values with <paramref name="comparison"/>.</summary>
        /// <param name="comparison">The comparison to use.</param>
        /// <returns>The new comparer.</returns>
        public static Comparer<T> Create(Comparison<T> comparison)
        {
            if (comparison == null) { throw new ArgumentNullException("comparison"); }
            return new ComparisonComparer<T>(comparison);
        }
#endif

        /// <summary>Compares two values.</summary>
        /// <param name="x">The first value.</param>
        /// <param name="y">The second value.</param>
        /// <returns>Less than zero when <paramref name="x"/> sorts before <paramref name="y"/>, zero
        /// when the two sort together, and greater than zero when it sorts after.</returns>
        public abstract int Compare(T x, T y);

        int IComparer.Compare(object x, object y)
        {
            if (x == null) { return y == null ? 0 : -1; }
            if (y == null) { return 1; }
            if (x is T && y is T) { return Compare((T)x, (T)y); }
            throw new ArgumentException("Type of argument is not compatible with the generic comparer.");
        }
    }

    internal sealed class ObjectComparer<T> : Comparer<T>
    {
        public override int Compare(T x, T y)
        {
            object left = x;
            object right = y;
            if (left == null) { return right == null ? 0 : -1; }
            if (right == null) { return 1; }
            IComparable<T> typed = left as IComparable<T>;
            if (typed != null && typeof(IComparable<T>).IsAssignableFrom(typeof(T)))
            {
                return typed.CompareTo(y);
            }
            return System.Collections.Comparer.Default.Compare(left, right);
        }
    }

    internal sealed class ComparisonComparer<T> : Comparer<T>
    {
        private readonly Comparison<T> comparison;

        internal ComparisonComparer(Comparison<T> comparison)
        {
            this.comparison = comparison;
        }

        public override int Compare(T x, T y)
        {
            return comparison(x, y);
        }
    }

    internal sealed class ArraySortHelper<T>
    {
        private const string ComparerFailed = "Failed to compare two elements in the array.";
        private const string InconsistentComparer = "Unable to sort because the IComparer.Compare() method returns inconsistent results. Either a value does not compare equal to itself, or one value repeatedly compared to another value yields different results. IComparer: '";
        private const int InsertionSortLimit = 16;

        private ArraySortHelper()
        {
        }

        internal static void Sort(T[] keys, int index, int length, IComparer<T> comparer)
        {
            IComparer<T> selected = comparer == null ? Comparer<T>.Default : comparer;
            bool genericOrder = (comparer == null || Object.ReferenceEquals(comparer, Comparer<T>.Default))
                && UsesGenericComparison(keys, index, length);
            Sort(keys, index, length, selected, comparer, genericOrder);
        }

        internal static void Sort(T[] keys, int index, int length, Comparison<T> comparison)
        {
            Sort(keys, index, length, new ComparisonComparer<T>(comparison), comparison, false);
        }

        private static bool UsesGenericComparison(T[] keys, int index, int length)
        {
            for (int i = index; i < index + length; i++)
            {
                object item = keys[i];
                if (item != null)
                {
                    return item is IComparable<T>
                        && typeof(IComparable<T>).IsAssignableFrom(typeof(T));
                }
            }
            return false;
        }

        // Default generic sorting calls the element's comparison directly, including a null
        // right operand. Null handling belongs to each sorting phase, not to this comparison.
        private static int CompareForSort(T left, T right, IComparer<T> comparer, bool genericOrder)
        {
            if (genericOrder) { return ((IComparable<T>)(object)left).CompareTo(right); }
            return comparer.Compare(left, right);
        }

        private static void Sort(T[] keys, int index, int length, IComparer<T> comparer, object named, bool genericOrder)
        {
            if (length < 2) { return; }
            try
            {
                IntroSort(keys, index, index + length - 1, 2 * (Log2(length) + 1), comparer, genericOrder);
            }
            catch (IndexOutOfRangeException)
            {
                throw new ArgumentException(InconsistentComparer + named + "'.");
            }
            catch (Exception e)
            {
                throw new InvalidOperationException(ComparerFailed, e);
            }
        }

        internal static int BinarySearch(T[] keys, int index, int length, T value, IComparer<T> comparer)
        {
            if (comparer == null) { comparer = Comparer<T>.Default; }
            try
            {
                int lo = index;
                int hi = index + length - 1;
                while (lo <= hi)
                {
                    int mid = lo + ((hi - lo) >> 1);
                    int order = comparer.Compare(keys[mid], value);
                    if (order == 0) { return mid; }
                    if (order < 0) { lo = mid + 1; } else { hi = mid - 1; }
                }
                return ~lo;
            }
            catch (Exception e)
            {
                throw new InvalidOperationException(ComparerFailed, e);
            }
        }

        private static int Log2(int value)
        {
            int log = 0;
            while (value > 1)
            {
                value = value >> 1;
                log = log + 1;
            }
            return log;
        }

        private static void IntroSort(T[] keys, int lo, int high, int depth, IComparer<T> comparer, bool genericOrder)
        {
            int hi = high;
            int depthLimit = depth;
            while (hi > lo)
            {
                int partitionSize = hi - lo + 1;
                if (partitionSize <= InsertionSortLimit)
                {
                    if (partitionSize == 2)
                    {
                        SwapIfGreater(keys, comparer, lo, hi, genericOrder);
                        return;
                    }
                    if (partitionSize == 3)
                    {
                        SwapIfGreater(keys, comparer, lo, hi - 1, genericOrder);
                        SwapIfGreater(keys, comparer, lo, hi, genericOrder);
                        SwapIfGreater(keys, comparer, hi - 1, hi, genericOrder);
                        return;
                    }
                    InsertionSort(keys, lo, hi, comparer, genericOrder);
                    return;
                }
                if (depthLimit == 0)
                {
                    HeapSort(keys, lo, hi, comparer, genericOrder);
                    return;
                }
                depthLimit = depthLimit - 1;
                int pivot = PickPivotAndPartition(keys, lo, hi, comparer, genericOrder);
                IntroSort(keys, pivot + 1, hi, depthLimit, comparer, genericOrder);
                hi = pivot - 1;
            }
        }

        private static void SwapIfGreater(T[] keys, IComparer<T> comparer, int i, int j, bool genericOrder)
        {
            if (i != j && (!genericOrder || (object)keys[i] != null)
                && CompareForSort(keys[i], keys[j], comparer, genericOrder) > 0)
            {
                T swap = keys[i];
                keys[i] = keys[j];
                keys[j] = swap;
            }
        }

        private static void Swap(T[] keys, int i, int j)
        {
            T swap = keys[i];
            keys[i] = keys[j];
            keys[j] = swap;
        }

        private static T InPartition(T[] keys, int i, int lo, int hi)
        {
            if (i < lo || i > hi) { throw new IndexOutOfRangeException(); }
            return keys[i];
        }

        private static int PickPivotAndPartition(T[] keys, int lo, int hi, IComparer<T> comparer, bool genericOrder)
        {
            int middle = lo + ((hi - lo) >> 1);
            SwapIfGreater(keys, comparer, lo, middle, genericOrder);
            SwapIfGreater(keys, comparer, lo, hi, genericOrder);
            SwapIfGreater(keys, comparer, middle, hi, genericOrder);
            T pivot = keys[middle];
            Swap(keys, middle, hi - 1);
            int left = lo;
            int right = hi - 1;
            while (left < right)
            {
                if (genericOrder)
                {
                    while (left < hi - 1)
                    {
                        left = left + 1;
                        T value = InPartition(keys, left, lo, hi);
                        if ((object)pivot == null)
                        {
                            if ((object)value != null) { break; }
                        }
                        else if (CompareForSort(pivot, value, comparer, true) <= 0) { break; }
                    }
                    while (right > lo)
                    {
                        right = right - 1;
                        T value = InPartition(keys, right, lo, hi);
                        if ((object)pivot == null)
                        {
                            if ((object)value == null) { break; }
                        }
                        else if (CompareForSort(pivot, value, comparer, true) >= 0) { break; }
                    }
                }
                else
                {
                    left = left + 1;
                    while (comparer.Compare(InPartition(keys, left, lo, hi), pivot) < 0) { left = left + 1; }
                    right = right - 1;
                    while (comparer.Compare(pivot, InPartition(keys, right, lo, hi)) < 0) { right = right - 1; }
                }
                if (left >= right) { break; }
                Swap(keys, left, right);
            }
            if (left != hi - 1) { Swap(keys, left, hi - 1); }
            return left;
        }

        private static void InsertionSort(T[] keys, int lo, int hi, IComparer<T> comparer, bool genericOrder)
        {
            int i = lo;
            while (i < hi)
            {
                int j = i;
                T next = keys[i + 1];
                while (j >= lo && ((genericOrder && (object)next == null)
                    || CompareForSort(next, keys[j], comparer, genericOrder) < 0))
                {
                    keys[j + 1] = keys[j];
                    j = j - 1;
                }
                keys[j + 1] = next;
                i = i + 1;
            }
        }

        private static void HeapSort(T[] keys, int lo, int hi, IComparer<T> comparer, bool genericOrder)
        {
            int n = hi - lo + 1;
            int i = n >> 1;
            while (i >= 1)
            {
                DownHeap(keys, i, n, lo, comparer, genericOrder);
                i = i - 1;
            }
            i = n;
            while (i > 1)
            {
                Swap(keys, lo, lo + i - 1);
                DownHeap(keys, 1, i - 1, lo, comparer, genericOrder);
                i = i - 1;
            }
        }

        private static void DownHeap(T[] keys, int start, int n, int lo, IComparer<T> comparer, bool genericOrder)
        {
            int i = start;
            T top = keys[lo + i - 1];
            while (i <= (n >> 1))
            {
                int child = 2 * i;
                if (child < n && ((genericOrder && (object)keys[lo + child - 1] == null)
                    || CompareForSort(keys[lo + child - 1], keys[lo + child], comparer, genericOrder) < 0)) { child = child + 1; }
                if ((genericOrder && (object)keys[lo + child - 1] == null)
                    || !(CompareForSort(top, keys[lo + child - 1], comparer, genericOrder) < 0)) { break; }
                keys[lo + i - 1] = keys[lo + child - 1];
                i = child;
            }
            keys[lo + i - 1] = top;
        }
    }
}
#endif
