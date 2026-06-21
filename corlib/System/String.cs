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

        public static string Concat(string a, string b, string c, string d)
        {
            System.Text.StringBuilder result = new System.Text.StringBuilder();
            if ((object)a != null) result.Append(a);
            if ((object)b != null) result.Append(b);
            if ((object)c != null) result.Append(c);
            if ((object)d != null) result.Append(d);
            return result.ToString();
        }

        public static string Concat(string[] values)
        {
            if ((object)values == null) throw new ArgumentNullException("values");
            System.Text.StringBuilder result = new System.Text.StringBuilder();
            for (int i = 0; i < values.Length; i++)
            {
                string value = values[i];
                if ((object)value != null) result.Append(value);
            }
            return result.ToString();
        }

        public static string Concat(object[] args)
        {
            if ((object)args == null) throw new ArgumentNullException("args");
            System.Text.StringBuilder result = new System.Text.StringBuilder();
            for (int i = 0; i < args.Length; i++)
            {
                object arg = args[i];
                if (arg != null) result.Append(arg.ToString());
            }
            return result.ToString();
        }

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
            if (startIndex < 0 || startIndex > n) throw new ArgumentOutOfRangeException("startIndex");
            for (int i = startIndex; i < n; i++)
            {
                if (this[i] == value) return i;
            }
            return -1;
        }

        public int IndexOfAny(char[] anyOf)
        {
            if ((object)anyOf == null) throw new ArgumentNullException("anyOf");
            int n = this.Length;
            for (int i = 0; i < n; i++)
            {
                char c = this[i];
                for (int j = 0; j < anyOf.Length; j++)
                {
                    if (anyOf[j] == c) return i;
                }
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

        public char[] ToCharArray(int startIndex, int length)
        {
            if (startIndex < 0) throw new ArgumentOutOfRangeException("startIndex");
            if (length < 0) throw new ArgumentOutOfRangeException("length");
            if (startIndex > this.Length - length) throw new ArgumentOutOfRangeException("startIndex");
            char[] result = new char[length];
            for (int i = 0; i < length; i++) result[i] = this[startIndex + i];
            return result;
        }

        public string Trim()
        {
            int start = 0;
            int end = this.Length - 1;
            while (start <= end && Char.IsWhiteSpace(this[start])) start++;
            while (end >= start && Char.IsWhiteSpace(this[end])) end--;
            if (start > end) return "";
            return this.Substring(start, end - start + 1);
        }

        private static bool IsTrimmable(char c, char[] trimChars)
        {
            if ((object)trimChars == null || trimChars.Length == 0) return Char.IsWhiteSpace(c);
            for (int i = 0; i < trimChars.Length; i++)
            {
                if (trimChars[i] == c) return true;
            }
            return false;
        }

        public string Trim(char[] trimChars)
        {
            int start = 0;
            int end = this.Length - 1;
            while (start <= end && IsTrimmable(this[start], trimChars)) start++;
            while (end >= start && IsTrimmable(this[end], trimChars)) end--;
            if (start > end) return "";
            return this.Substring(start, end - start + 1);
        }

        public string TrimStart(char[] trimChars)
        {
            int start = 0;
            int n = this.Length;
            while (start < n && IsTrimmable(this[start], trimChars)) start++;
            return this.Substring(start);
        }

        public string TrimEnd(char[] trimChars)
        {
            int end = this.Length - 1;
            while (end >= 0 && IsTrimmable(this[end], trimChars)) end--;
            return this.Substring(0, end + 1);
        }

        public string ToUpper()
        {
            System.Text.StringBuilder result = new System.Text.StringBuilder();
            int n = this.Length;
            for (int i = 0; i < n; i++) result.Append(Char.ToUpper(this[i]));
            return result.ToString();
        }

        public string ToLower()
        {
            System.Text.StringBuilder result = new System.Text.StringBuilder();
            int n = this.Length;
            for (int i = 0; i < n; i++) result.Append(Char.ToLower(this[i]));
            return result.ToString();
        }

        public string Replace(char oldChar, char newChar)
        {
            System.Text.StringBuilder result = new System.Text.StringBuilder();
            int n = this.Length;
            for (int i = 0; i < n; i++)
            {
                char c = this[i];
                result.Append(c == oldChar ? newChar : c);
            }
            return result.ToString();
        }

        public string Replace(string oldValue, string newValue)
        {
            if ((object)oldValue == null) throw new ArgumentNullException("oldValue");
            int oldLength = oldValue.Length;
            if (oldLength == 0) throw new ArgumentException("String cannot be of zero length.");
            System.Text.StringBuilder result = new System.Text.StringBuilder();
            int n = this.Length;
            int i = 0;
            while (i < n)
            {
                if (i <= n - oldLength && this.MatchesAt(oldValue, i))
                {
                    if ((object)newValue != null) result.Append(newValue);
                    i += oldLength;
                }
                else
                {
                    result.Append(this[i]);
                    i++;
                }
            }
            return result.ToString();
        }

        private bool MatchesAt(string value, int start)
        {
            int m = value.Length;
            for (int j = 0; j < m; j++)
            {
                if (this[start + j] != value[j]) return false;
            }
            return true;
        }

        public string PadLeft(int totalWidth) { return PadLeft(totalWidth, ' '); }

        public string PadLeft(int totalWidth, char paddingChar)
        {
            if (totalWidth < 0) throw new ArgumentOutOfRangeException("totalWidth");
            int n = this.Length;
            if (totalWidth <= n) return this;
            System.Text.StringBuilder result = new System.Text.StringBuilder();
            for (int i = 0; i < totalWidth - n; i++) result.Append(paddingChar);
            for (int i = 0; i < n; i++) result.Append(this[i]);
            return result.ToString();
        }

        public string PadRight(int totalWidth) { return PadRight(totalWidth, ' '); }

        public string PadRight(int totalWidth, char paddingChar)
        {
            if (totalWidth < 0) throw new ArgumentOutOfRangeException("totalWidth");
            int n = this.Length;
            if (totalWidth <= n) return this;
            System.Text.StringBuilder result = new System.Text.StringBuilder();
            for (int i = 0; i < n; i++) result.Append(this[i]);
            for (int i = 0; i < totalWidth - n; i++) result.Append(paddingChar);
            return result.ToString();
        }

        public string Insert(int startIndex, string value)
        {
            if ((object)value == null) throw new ArgumentNullException("value");
            int n = this.Length;
            if (startIndex < 0 || startIndex > n) throw new ArgumentOutOfRangeException("startIndex");
            System.Text.StringBuilder result = new System.Text.StringBuilder();
            for (int i = 0; i < startIndex; i++) result.Append(this[i]);
            result.Append(value);
            for (int i = startIndex; i < n; i++) result.Append(this[i]);
            return result.ToString();
        }

        public string Remove(int startIndex)
        {
            int n = this.Length;
            if (startIndex < 0 || startIndex > n) throw new ArgumentOutOfRangeException("startIndex");
            return this.Substring(0, startIndex);
        }

        public string Remove(int startIndex, int count)
        {
            if (startIndex < 0) throw new ArgumentOutOfRangeException("startIndex");
            if (count < 0) throw new ArgumentOutOfRangeException("count");
            int n = this.Length;
            if (startIndex > n - count) throw new ArgumentOutOfRangeException("count");
            System.Text.StringBuilder result = new System.Text.StringBuilder();
            for (int i = 0; i < startIndex; i++) result.Append(this[i]);
            for (int i = startIndex + count; i < n; i++) result.Append(this[i]);
            return result.ToString();
        }

        private static int CompareOrdinalNormalized(string a, string b)
        {
            if ((object)a == null) return (object)b == null ? 0 : -1;
            if ((object)b == null) return 1;
            int n = a.Length;
            int m = b.Length;
            int limit = n < m ? n : m;
            for (int i = 0; i < limit; i++)
            {
                if (a[i] != b[i]) return a[i] < b[i] ? -1 : 1;
            }
            if (n == m) return 0;
            return n < m ? -1 : 1;
        }

        public int CompareTo(string strB) { return CompareOrdinalNormalized(this, strB); }

        public int CompareTo(object obj)
        {
            if ((object)obj == null) return 1;
            string other = obj as string;
            if ((object)other == null) throw new ArgumentException("Object must be of type String.");
            return CompareOrdinalNormalized(this, other);
        }

        public static int Compare(string strA, string strB) { return CompareOrdinalNormalized(strA, strB); }

        public static int CompareOrdinal(string strA, string strB)
        {
            if ((object)strA == null) return (object)strB == null ? 0 : -1;
            if ((object)strB == null) return 1;
            int n = strA.Length;
            int m = strB.Length;
            int limit = n < m ? n : m;
            for (int i = 0; i < limit; i++)
            {
                int diff = strA[i] - strB[i];
                if (diff != 0) return diff;
            }
            return n - m;
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
