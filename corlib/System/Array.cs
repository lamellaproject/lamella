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

        public bool IsFixedSize { get { return true; } }
        public bool IsReadOnly { get { return false; } }
        public bool IsSynchronized { get { return false; } }
        public object SyncRoot { get { return this; } }

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

        public static int IndexOf(Array array, object value, int startIndex)
        {
            if ((object)array == null) throw new ArgumentNullException("array");
            return IndexOf(array, value, startIndex, array.Length - startIndex);
        }

        public static int IndexOf(Array array, object value, int startIndex, int count)
        {
            if ((object)array == null) throw new ArgumentNullException("array");
            int n = array.Length;
            if (startIndex < 0 || startIndex > n) throw new ArgumentOutOfRangeException("startIndex");
            if (count < 0 || startIndex > n - count) throw new ArgumentOutOfRangeException("count");
            int end = startIndex + count;
            for (int i = startIndex; i < end; i++)
            {
                object element = array.GetValue(i);
                if (value == null) { if (element == null) return i; }
                else if (element != null && element.Equals(value)) return i;
            }
            return -1;
        }

        public static int LastIndexOf(Array array, object value)
        {
            if ((object)array == null) throw new ArgumentNullException("array");
            return LastIndexOf(array, value, array.Length - 1);
        }

        public static int LastIndexOf(Array array, object value, int startIndex)
        {
            if ((object)array == null) throw new ArgumentNullException("array");
            int n = array.Length;
            if (n == 0) return -1;
            if (startIndex < 0 || startIndex >= n) throw new ArgumentOutOfRangeException("startIndex");
            for (int i = startIndex; i >= 0; i--)
            {
                object element = array.GetValue(i);
                if (value == null) { if (element == null) return i; }
                else if (element != null && element.Equals(value)) return i;
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
            Copy(sourceArray, 0, destinationArray, 0, length);
        }

        public static void Copy(Array sourceArray, int sourceIndex, Array destinationArray, int destinationIndex, int length)
        {
            if ((object)sourceArray == null) throw new ArgumentNullException("sourceArray");
            if ((object)destinationArray == null) throw new ArgumentNullException("destinationArray");
            if (sourceIndex < 0) throw new ArgumentOutOfRangeException("sourceIndex");
            if (destinationIndex < 0) throw new ArgumentOutOfRangeException("destinationIndex");
            if (length < 0) throw new ArgumentOutOfRangeException("length");
            if (sourceIndex > sourceArray.Length - length) throw new ArgumentException("sourceArray");
            if (destinationIndex > destinationArray.Length - length) throw new ArgumentException("destinationArray");
            if (CopyCore(sourceArray, sourceIndex, destinationArray, destinationIndex, length)) return;
            bool backward = (object)sourceArray == (object)destinationArray && destinationIndex > sourceIndex;
            if (backward)
            {
                for (int i = length - 1; i >= 0; i--)
                    destinationArray.SetValue(sourceArray.GetValue(sourceIndex + i), destinationIndex + i);
            }
            else
            {
                for (int i = 0; i < length; i++)
                    destinationArray.SetValue(sourceArray.GetValue(sourceIndex + i), destinationIndex + i);
            }
        }

        public void CopyTo(Array array, int index)
        {
            if ((object)array != null && array.Rank != 1) throw new ArgumentException("array");
            Copy(this, GetLowerBound(0), array, index, Length);
        }

        [Lamella.Runtime.RuntimeProvided]
        private static bool CopyCore(Array source, int sourceIndex, Array destination, int destinationIndex, int length) { return false; }

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

        public static Array CreateInstance(Type elementType, int length)
        {
            if ((object)elementType == null) throw new ArgumentNullException("elementType");
            if (length < 0) throw new ArgumentOutOfRangeException("length");
            return CreateInstanceCore(elementType, length);
        }

        [Lamella.Runtime.RuntimeProvided] private static Array CreateInstanceCore(Type elementType, int length) { return null; }

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
            return BinarySearch(array, 0, array.Length, value, comparer);
        }

        public static int BinarySearch(Array array, int index, int length, object value, System.Collections.IComparer comparer)
        {
            if (array == null) throw new ArgumentNullException("array");
            if (index < 0) throw new ArgumentOutOfRangeException("index");
            if (length < 0) throw new ArgumentOutOfRangeException("length");
            if (index > array.Length - length) throw new ArgumentException("array");
            if (comparer == null) comparer = System.Collections.Comparer.Default;
            int lo = index;
            int hi = index + length - 1;
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
