// Lamella managed corlib (from scratch). -- System.String
namespace System
{
    public sealed class String : IComparable
    {
        public int Length { [Lamella.Runtime.RuntimeProvided] get { return 0; } }
        [System.Runtime.CompilerServices.IndexerName("Chars")]
        public char this[int index] { [Lamella.Runtime.RuntimeProvided] get { return '\0'; } }

        [Lamella.Runtime.RuntimeProvided] public string Substring(int startIndex) { return null; }
        [Lamella.Runtime.RuntimeProvided] public string Substring(int startIndex, int length) { return null; }

        [Lamella.Runtime.RuntimeProvided] public static string Concat(string a, string b) { return null; }

        public static bool operator ==(string a, string b)
        {
            if ((object)a == (object)b) return true;
            if ((object)a == null) return false;
            if ((object)b == null) return false;
            int n = a.Length;
            if (n != b.Length) return false;
            for (int i = 0; i < n; i++)
            {
                if (a[i] != b[i]) return false;
            }
            return true;
        }
        public static bool operator !=(string a, string b) { return !(a == b); }

        public static bool IsNullOrEmpty(string value)
        {
            if ((object)value == null) return true;
            return value.Length == 0;
        }

        public int IndexOf(char value)
        {
            int n = this.Length;
            for (int i = 0; i < n; i++)
            {
                if (this[i] == value) return i;
            }
            return -1;
        }

        public int IndexOf(char value, int startIndex)
        {
            int n = this.Length;
            for (int i = startIndex; i < n; i++)
            {
                if (this[i] == value) return i;
            }
            return -1;
        }

        public int LastIndexOf(char value)
        {
            for (int i = this.Length - 1; i >= 0; i--)
            {
                if (this[i] == value) return i;
            }
            return -1;
        }

        public bool StartsWith(string value)
        {
            int n = value.Length;
            if (n > this.Length) return false;
            for (int i = 0; i < n; i++)
            {
                if (this[i] != value[i]) return false;
            }
            return true;
        }

        public bool EndsWith(string value)
        {
            int n = value.Length;
            int offset = this.Length - n;
            if (offset < 0) return false;
            for (int i = 0; i < n; i++)
            {
                if (this[offset + i] != value[i]) return false;
            }
            return true;
        }

        public char[] ToCharArray()
        {
            char[] result = new char[this.Length];
            for (int i = 0; i < result.Length; i++) result[i] = this[i];
            return result;
        }

        public string Trim()
        {
            int start = 0;
            int end = this.Length - 1;
            while (start <= end && Char.IsWhiteSpace(this[start])) start++;
            while (end >= start && Char.IsWhiteSpace(this[end])) end--;
            return this.Substring(start, end - start + 1);
        }

        public int CompareTo(object obj)
        {
            if ((object)obj == null) return 1;
            string other = (string)obj;
            int n = this.Length;
            int m = other.Length;
            int limit = n < m ? n : m;
            for (int i = 0; i < limit; i++)
            {
                int diff = this[i] - other[i];
                if (diff != 0) return diff < 0 ? -1 : 1;
            }
            if (n < m) return -1;
            if (n > m) return 1;
            return 0;
        }

        public bool Equals(string value) { return this == value; }

        public override bool Equals(object value)
        {
            string other = value as string;
            if ((object)other == null) return false;
            return this == other;
        }

        public override int GetHashCode()
        {
            int hash = 0;
            int n = this.Length;
            for (int i = 0; i < n; i++) hash = hash * 31 + this[i];
            return hash;
        }

        public override string ToString() { return this; }
    }
}
