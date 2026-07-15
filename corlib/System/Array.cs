// Lamella managed corlib (from scratch). -- System.Array
namespace System
{
    public abstract class Array : ICloneable
    {
        public int Length { [Lamella.Runtime.RuntimeProvided] get { return 0; } }
        public int Rank { [Lamella.Runtime.RuntimeProvided] get { return 0; } }
        [Lamella.Runtime.RuntimeProvided] public int GetLength(int dimension) { return 0; }

        public int GetLowerBound(int dimension)
        {
            GetLength(dimension);
            return 0;
        }

        public int GetUpperBound(int dimension)
        {
            return GetLength(dimension) - 1;
        }
        [Lamella.Runtime.RuntimeProvided] public object GetValue(int index) { return null; }
        [Lamella.Runtime.RuntimeProvided] public void SetValue(object value, int index) { }

        public System.Collections.IEnumerator GetEnumerator()
        {
            return new ArrayEnumerator(this);
        }

        [Lamella.Runtime.RuntimeProvided] public object Clone() { return null; }

        public static int IndexOf(Array array, object value)
        {
            int n = array.Length;
            for (int i = 0; i < n; i++)
            {
                object element = array.GetValue(i);
                if (value == null)
                {
                    if (element == null) return i;
                }
                else if (element != null && element.Equals(value))
                {
                    return i;
                }
            }
            return -1;
        }

        public static void Reverse(Array array)
        {
            int i = 0;
            int j = array.Length - 1;
            while (i < j)
            {
                object tmp = array.GetValue(i);
                array.SetValue(array.GetValue(j), i);
                array.SetValue(tmp, j);
                i = i + 1;
                j = j - 1;
            }
        }

        public static void Copy(Array sourceArray, Array destinationArray, int length)
        {
            for (int i = 0; i < length; i++)
            {
                destinationArray.SetValue(sourceArray.GetValue(i), i);
            }
        }

        public static void Clear(Array array, int index, int length)
        {
            if ((object)array == null) throw new ArgumentNullException("array");
            if (index < 0 || length < 0 || index > array.Length - length)
            {
                throw new IndexOutOfRangeException();
            }
            ClearCore(array, index, length);
        }

        [Lamella.Runtime.RuntimeProvided] private static void ClearCore(Array array, int index, int length) { }

        public static void Sort(Array array)
        {
            int n = array.Length;
            for (int i = 1; i < n; i++)
            {
                object key = array.GetValue(i);
                IComparable keyComparable = (IComparable)key;
                int j = i - 1;
                while (j >= 0 && keyComparable.CompareTo(array.GetValue(j)) < 0)
                {
                    array.SetValue(array.GetValue(j), j + 1);
                    j = j - 1;
                }
                array.SetValue(key, j + 1);
            }
        }

        public static int BinarySearch(Array array, object value)
        {
            return BinarySearch(array, value, System.Collections.Comparer.Default);
        }

        public static int BinarySearch(Array array, object value, System.Collections.IComparer comparer)
        {
            if (array == null) throw new ArgumentNullException("array");
            if (comparer == null) comparer = System.Collections.Comparer.Default;
            int lo = 0;
            int hi = array.Length - 1;
            while (lo <= hi)
            {
                int mid = lo + ((hi - lo) >> 1);
                int order = comparer.Compare(array.GetValue(mid), value);
                if (order == 0) return mid;
                if (order < 0) lo = mid + 1;
                else hi = mid - 1;
            }
            return ~lo;
        }
    }

    internal sealed class ArrayEnumerator : System.Collections.IEnumerator
    {
        private readonly Array array;
        private int index;

        internal ArrayEnumerator(Array array)
        {
            this.array = array;
            this.index = -1;
        }

        public bool MoveNext()
        {
            if (index + 1 >= array.Length) return false;
            index = index + 1;
            return true;
        }

        public object Current
        {
            get
            {
                if (index < 0 || index >= array.Length) throw new InvalidOperationException("Enumeration has either not started or has already finished.");
                return array.GetValue(index);
            }
        }

        public void Reset()
        {
            index = -1;
        }
    }
}
