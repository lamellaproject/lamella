// Lamella managed corlib (from scratch). -- System.Collections.Comparer
namespace System.Collections
{
    public sealed class Comparer : IComparer
    {
        public static readonly Comparer Default = new Comparer();

        private Comparer() { }

        public int Compare(object x, object y)
        {
            if (x == y) return 0;
            if (x == null) return -1;
            if (y == null) return 1;
            IComparable comparable = x as IComparable;
            if (comparable == null) throw new ArgumentException("At least one object must implement IComparable.");
            return comparable.CompareTo(y);
        }
    }
}
