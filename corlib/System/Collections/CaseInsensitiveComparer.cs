// Lamella managed corlib (from scratch). -- System.Collections.CaseInsensitiveComparer
namespace System.Collections
{
    public class CaseInsensitiveComparer : IComparer
    {
        private static CaseInsensitiveComparer _default = new CaseInsensitiveComparer();

        public CaseInsensitiveComparer() { }

        public static CaseInsensitiveComparer Default { get { return _default; } }

        public int Compare(object a, object b)
        {
            string sa = a as string;
            string sb = b as string;
            if ((object)sa != null && (object)sb != null) return sa.ToUpper().CompareTo(sb.ToUpper());
            return Comparer.Default.Compare(a, b);
        }
    }
}
