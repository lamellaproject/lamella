// Lamella managed corlib (from scratch). -- System.String
namespace System
{
    public sealed class String : IComparable, ICloneable
    {
        public static readonly string Empty = "";

        [Lamella.Runtime.RuntimeProvided] public unsafe String(char* value) { }
        [Lamella.Runtime.RuntimeProvided] public unsafe String(char* value, int startIndex, int length) { }

        [Lamella.Runtime.RuntimeProvided] public String(char[] value) { }
        [Lamella.Runtime.RuntimeProvided] public String(char[] value, int startIndex, int length) { }
        [Lamella.Runtime.RuntimeProvided] public String(char c, int count) { }

        public int Length { [Lamella.Runtime.RuntimeProvided] get { return 0; } }
        [System.Runtime.CompilerServices.IndexerName("Chars")]
        public char this[int index] { [Lamella.Runtime.RuntimeProvided] get { return '\0'; } }

        [Lamella.Runtime.RuntimeProvided] public string Substring(int startIndex) { return null; }
        [Lamella.Runtime.RuntimeProvided] public string Substring(int startIndex, int length) { return null; }

        [Lamella.Runtime.RuntimeProvided] public static string CreateFromChars(char[] chars, int start, int len) { return null; }

        [Lamella.Runtime.RuntimeProvided] public static string Concat(string a, string b) { return null; }

        public static string Concat(string a, string b, string c)
        {
            System.Text.StringBuilder result = new System.Text.StringBuilder();
            if ((object)a != null) result.Append(a);
            if ((object)b != null) result.Append(b);
            if ((object)c != null) result.Append(c);
            return result.ToString();
        }

        public static string Concat(string a, string b, string c, string d)
        {
            System.Text.StringBuilder result = new System.Text.StringBuilder();
            if ((object)a != null) result.Append(a);
            if ((object)b != null) result.Append(b);
            if ((object)c != null) result.Append(c);
            if ((object)d != null) result.Append(d);
            return result.ToString();
        }

        private static string ObjectText(object value)
        {
            if (value == null) return "";
            return value.ToString();
        }

        public static string Concat(object arg0)
        {
            return ObjectText(arg0);
        }

        public static string Concat(object arg0, object arg1)
        {
            return Concat(ObjectText(arg0), ObjectText(arg1));
        }

        public static string Concat(object arg0, object arg1, object arg2)
        {
            return Concat(ObjectText(arg0), ObjectText(arg1), ObjectText(arg2));
        }

        public static string Concat(params string[] values)
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

        public static string Concat(params object[] args)
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

        public static string Format(string format, object arg0)
        {
            return Format(format, new object[] { arg0 });
        }

        public static string Format(string format, object arg0, object arg1)
        {
            return Format(format, new object[] { arg0, arg1 });
        }

        public static string Format(string format, object arg0, object arg1, object arg2)
        {
            return Format(format, new object[] { arg0, arg1, arg2 });
        }

        public static string Format(string format, params object[] args)
        {
            if ((object)format == null) throw new ArgumentNullException("format");
            if ((object)args == null) throw new ArgumentNullException("args");
            System.Text.StringBuilder result = new System.Text.StringBuilder();
            int n = format.Length;
            int pos = 0;
            while (pos < n)
            {
                char c = format[pos];
                if (c == '}')
                {
                    if (pos + 1 < n && format[pos + 1] == '}')
                    {
                        result.Append('}');
                        pos += 2;
                    }
                    else
                    {
                        throw new FormatException("Input string was not in a correct format.");
                    }
                }
                else if (c == '{')
                {
                    if (pos + 1 < n && format[pos + 1] == '{')
                    {
                        result.Append('{');
                        pos += 2;
                    }
                    else
                    {
                        pos = AppendFormatItem(result, format, pos, args);
                    }
                }
                else
                {
                    result.Append(c);
                    pos++;
                }
            }
            return result.ToString();
        }

        private static int AppendFormatItem(System.Text.StringBuilder result, string format, int start, object[] args)
        {
            int n = format.Length;
            int pos = start + 1;
            if (pos >= n || format[pos] < '0' || format[pos] > '9')
            {
                throw new FormatException("Input string was not in a correct format.");
            }
            int index = 0;
            while (pos < n && format[pos] >= '0' && format[pos] <= '9')
            {
                index = index * 10 + (format[pos] - '0');
                pos++;
            }
            int alignment = 0;
            if (pos < n && format[pos] == ',')
            {
                pos++;
                bool negative = false;
                if (pos < n && format[pos] == '-') { negative = true; pos++; }
                if (pos >= n || format[pos] < '0' || format[pos] > '9')
                {
                    throw new FormatException("Input string was not in a correct format.");
                }
                int width = 0;
                while (pos < n && format[pos] >= '0' && format[pos] <= '9')
                {
                    width = width * 10 + (format[pos] - '0');
                    pos++;
                }
                alignment = negative ? -width : width;
            }
            string itemFormat = null;
            if (pos < n && format[pos] == ':')
            {
                pos++;
                int specStart = pos;
                while (pos < n && format[pos] != '}') pos++;
                itemFormat = format.Substring(specStart, pos - specStart);
            }
            if (pos >= n || format[pos] != '}')
            {
                throw new FormatException("Input string was not in a correct format.");
            }
            pos++;
            if (index >= args.Length)
            {
                throw new FormatException("Index (zero based) must be greater than or equal to zero and less than the size of the argument list.");
            }
            object arg = args[index];
            string text;
            if (arg == null)
            {
                text = "";
            }
            else if ((object)itemFormat != null)
            {
                IFormattable formattable = arg as IFormattable;
                if ((object)formattable != null) text = formattable.ToString(itemFormat, null);
                else text = arg.ToString();
            }
            else
            {
                text = arg.ToString();
            }
            if (alignment > 0) text = text.PadLeft(alignment);
            else if (alignment < 0) text = text.PadRight(-alignment);
            result.Append(text);
            return pos;
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

#if LAMELLA_SURFACE_NETFX_2_0
        public static bool IsNullOrEmpty(string value)
        {
            if ((object)value == null) return true;
            return value.Length == 0;
        }
#endif

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

        public int IndexOf(string value)
        {
            return IndexOf(value, 0);
        }

        public int IndexOf(string value, int startIndex)
        {
            if ((object)value == null) throw new ArgumentNullException("value");
            int n = this.Length;
            int m = value.Length;
            if (startIndex < 0 || startIndex > n) throw new ArgumentOutOfRangeException("startIndex");
            if (m == 0) return startIndex;
            for (int i = startIndex; i <= n - m; i++)
            {
                bool match = true;
                for (int j = 0; j < m; j++)
                {
                    if (this[i + j] != value[j]) { match = false; break; }
                }
                if (match) return i;
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

        public int IndexOf(char value, int startIndex, int count)
        {
            int n = this.Length;
            if (startIndex < 0 || startIndex > n) throw new ArgumentOutOfRangeException("startIndex");
            if (count < 0 || startIndex > n - count) throw new ArgumentOutOfRangeException("count");
            int end = startIndex + count;
            for (int i = startIndex; i < end; i++)
            {
                if (this[i] == value) return i;
            }
            return -1;
        }

        public int IndexOf(string value, int startIndex, int count)
        {
            if ((object)value == null) throw new ArgumentNullException("value");
            int n = this.Length;
            if (startIndex < 0 || startIndex > n) throw new ArgumentOutOfRangeException("startIndex");
            if (count < 0 || startIndex > n - count) throw new ArgumentOutOfRangeException("count");
            int m = value.Length;
            if (m == 0) return startIndex;
            int last = startIndex + count - m;
            for (int i = startIndex; i <= last; i++)
            {
                bool match = true;
                for (int j = 0; j < m; j++)
                {
                    if (this[i + j] != value[j]) { match = false; break; }
                }
                if (match) return i;
            }
            return -1;
        }

        public int IndexOfAny(char[] anyOf, int startIndex)
        {
            if ((object)anyOf == null) throw new ArgumentNullException("anyOf");
            int n = this.Length;
            if (startIndex < 0 || startIndex > n) throw new ArgumentOutOfRangeException("startIndex");
            for (int i = startIndex; i < n; i++)
            {
                char c = this[i];
                for (int j = 0; j < anyOf.Length; j++)
                {
                    if (anyOf[j] == c) return i;
                }
            }
            return -1;
        }

        public int IndexOfAny(char[] anyOf, int startIndex, int count)
        {
            if ((object)anyOf == null) throw new ArgumentNullException("anyOf");
            int n = this.Length;
            if (startIndex < 0 || startIndex > n) throw new ArgumentOutOfRangeException("startIndex");
            if (count < 0 || startIndex > n - count) throw new ArgumentOutOfRangeException("count");
            int end = startIndex + count;
            for (int i = startIndex; i < end; i++)
            {
                char c = this[i];
                for (int j = 0; j < anyOf.Length; j++)
                {
                    if (anyOf[j] == c) return i;
                }
            }
            return -1;
        }

        public int LastIndexOf(char value, int startIndex)
        {
            int n = this.Length;
            if (n == 0) return -1;
            if (startIndex < 0 || startIndex >= n) throw new ArgumentOutOfRangeException("startIndex");
            for (int i = startIndex; i >= 0; i--)
            {
                if (this[i] == value) return i;
            }
            return -1;
        }

        public int LastIndexOf(char value, int startIndex, int count)
        {
            int n = this.Length;
            if (n == 0) return -1;
            if (startIndex < 0 || startIndex >= n) throw new ArgumentOutOfRangeException("startIndex");
            if (count < 0 || startIndex - count + 1 < 0) throw new ArgumentOutOfRangeException("count");
            int floor = startIndex - count + 1;
            for (int i = startIndex; i >= floor; i--)
            {
                if (this[i] == value) return i;
            }
            return -1;
        }

        public int LastIndexOf(string value)
        {
            if ((object)value == null) throw new ArgumentNullException("value");
            int n = this.Length;
            if (n == 0) return value.Length == 0 ? 0 : -1;
            return LastIndexOf(value, n - 1, n);
        }

        public int LastIndexOf(string value, int startIndex)
        {
            if ((object)value == null) throw new ArgumentNullException("value");
            if (this.Length == 0 && (startIndex == -1 || startIndex == 0))
            {
                return value.Length == 0 ? 0 : -1;
            }
            return LastIndexOf(value, startIndex, startIndex + 1);
        }

        public int LastIndexOf(string value, int startIndex, int count)
        {
            if ((object)value == null) throw new ArgumentNullException("value");
            int n = this.Length;
            if (n == 0 && (startIndex == -1 || startIndex == 0)) return value.Length == 0 ? 0 : -1;
            if (startIndex < 0 || startIndex > n) throw new ArgumentOutOfRangeException("startIndex");
            if (count < 0 || startIndex - count + 1 < 0) throw new ArgumentOutOfRangeException("count");
            int m = value.Length;
            if (m == 0) return startIndex + 1 > n ? n : startIndex + 1;
            int floor = startIndex - count + 1;
            int start = startIndex - m + 1;
            if (start > n - m) start = n - m;
            for (int i = start; i >= floor; i--)
            {
                bool match = true;
                for (int j = 0; j < m; j++)
                {
                    if (this[i + j] != value[j]) { match = false; break; }
                }
                if (match) return i;
            }
            return -1;
        }

        public int LastIndexOfAny(char[] anyOf)
        {
            if ((object)anyOf == null) throw new ArgumentNullException("anyOf");
            for (int i = this.Length - 1; i >= 0; i--)
            {
                char c = this[i];
                for (int j = 0; j < anyOf.Length; j++)
                {
                    if (anyOf[j] == c) return i;
                }
            }
            return -1;
        }

        public int LastIndexOfAny(char[] anyOf, int startIndex)
        {
            if ((object)anyOf == null) throw new ArgumentNullException("anyOf");
            int n = this.Length;
            if (n == 0) return -1;
            if (startIndex < 0 || startIndex >= n) throw new ArgumentOutOfRangeException("startIndex");
            for (int i = startIndex; i >= 0; i--)
            {
                char c = this[i];
                for (int j = 0; j < anyOf.Length; j++)
                {
                    if (anyOf[j] == c) return i;
                }
            }
            return -1;
        }

        public int LastIndexOfAny(char[] anyOf, int startIndex, int count)
        {
            if ((object)anyOf == null) throw new ArgumentNullException("anyOf");
            int n = this.Length;
            if (n == 0) return -1;
            if (startIndex < 0 || startIndex >= n) throw new ArgumentOutOfRangeException("startIndex");
            if (count < 0 || startIndex - count + 1 < 0) throw new ArgumentOutOfRangeException("count");
            int floor = startIndex - count + 1;
            for (int i = startIndex; i >= floor; i--)
            {
                char c = this[i];
                for (int j = 0; j < anyOf.Length; j++)
                {
                    if (anyOf[j] == c) return i;
                }
            }
            return -1;
        }

        public static string Copy(string str)
        {
            if ((object)str == null) throw new ArgumentNullException("str");
            return new String(str.ToCharArray(), 0, str.Length);
        }

        public bool Contains(string value)
        {
            return IndexOf(value) >= 0;
        }

        public string[] Split(params char[] separator)
        {
            return Split(separator, Int32.MaxValue);
        }

        public string[] Split(char[] separator, int count)
        {
            if (count < 0) throw new ArgumentOutOfRangeException("count");
            if (count == 0) return new string[0];
            bool whitespace = (object)separator == null || separator.Length == 0;
            int n = this.Length;
            int pieces = 1;
            for (int i = 0; i < n && pieces < count; i++)
            {
                if (IsSeparatorChar(this[i], separator, whitespace)) pieces++;
            }
            string[] result = new string[pieces];
            int start = 0;
            int idx = 0;
            for (int i = 0; i < n && idx < pieces - 1; i++)
            {
                if (IsSeparatorChar(this[i], separator, whitespace))
                {
                    result[idx] = this.Substring(start, i - start);
                    idx++;
                    start = i + 1;
                }
            }
            result[pieces - 1] = this.Substring(start, n - start);
            return result;
        }

        private static bool IsSeparatorChar(char c, char[] separator, bool whitespace)
        {
            if (whitespace) return Char.IsWhiteSpace(c);
            for (int j = 0; j < separator.Length; j++)
            {
                if (separator[j] == c) return true;
            }
            return false;
        }

        public static string Join(string separator, string[] value)
        {
            if ((object)value == null) throw new ArgumentNullException("value");
            return Join(separator, value, 0, value.Length);
        }

        public static string Join(string separator, string[] value, int startIndex, int count)
        {
            if ((object)value == null) throw new ArgumentNullException("value");
            if (startIndex < 0) throw new ArgumentOutOfRangeException("startIndex");
            if (count < 0) throw new ArgumentOutOfRangeException("count");
            if (startIndex > value.Length - count) throw new ArgumentOutOfRangeException("startIndex");
            if ((object)separator == null) separator = "";
            if (count == 0) return "";
            string first = value[startIndex];
            string result = (object)first == null ? "" : first;
            for (int i = 1; i < count; i++)
            {
                string piece = value[startIndex + i];
                result = String.Concat(result, separator);
                result = String.Concat(result, (object)piece == null ? "" : piece);
            }
            return result;
        }

        public void CopyTo(int sourceIndex, char[] destination, int destinationIndex, int count)
        {
            if ((object)destination == null) throw new ArgumentNullException("destination");
            if (count < 0) throw new ArgumentOutOfRangeException("count");
            if (sourceIndex < 0 || sourceIndex > this.Length - count) throw new ArgumentOutOfRangeException("sourceIndex");
            if (destinationIndex < 0 || destinationIndex > destination.Length - count) throw new ArgumentOutOfRangeException("destinationIndex");
            for (int i = 0; i < count; i++)
            {
                destination[destinationIndex + i] = this[sourceIndex + i];
            }
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

        public string Trim(params char[] trimChars)
        {
            int start = 0;
            int end = this.Length - 1;
            while (start <= end && IsTrimmable(this[start], trimChars)) start++;
            while (end >= start && IsTrimmable(this[end], trimChars)) end--;
            if (start > end) return "";
            return this.Substring(start, end - start + 1);
        }

        public string TrimStart(params char[] trimChars)
        {
            int start = 0;
            int n = this.Length;
            while (start < n && IsTrimmable(this[start], trimChars)) start++;
            return this.Substring(start);
        }

        public string TrimEnd(params char[] trimChars)
        {
            int end = this.Length - 1;
            while (end >= 0 && IsTrimmable(this[end], trimChars)) end--;
            return this.Substring(0, end + 1);
        }

        public string ToUpper() { return MapCase(true); }

        public string ToLower() { return MapCase(false); }

        private string MapCase(bool toUpper)
        {
            System.Text.StringBuilder result = new System.Text.StringBuilder();
            int n = this.Length;
            for (int i = 0; i < n; i++)
            {
                char c = this[i];
                if (c >= 0xD800 && c <= 0xDBFF && i + 1 < n)
                {
                    char d = this[i + 1];
                    if (d >= 0xDC00 && d <= 0xDFFF)
                    {
                        int cp = 0x10000 + ((c - 0xD800) << 10) + (d - 0xDC00);
                        int mapped = toUpper ? CaseMapping.ToUpperCodePoint(cp) : CaseMapping.ToLowerCodePoint(cp);
                        result.Append((char)(0xD800 + ((mapped - 0x10000) >> 10)));
                        result.Append((char)(0xDC00 + ((mapped - 0x10000) & 0x3FF)));
                        i++;
                        continue;
                    }
                }
                result.Append(toUpper ? CaseMapping.ToUpper(c) : CaseMapping.ToLower(c));
            }
            return result.ToString();
        }
#if LAMELLA_SURFACE_STRING_COMPARISON
        public string ToUpperInvariant() { return MapCase(true); }

        public string ToLowerInvariant() { return MapCase(false); }
#endif

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

        public int CompareTo(string strB) { return CompareOrdinal(this, strB); }

        public int CompareTo(object obj)
        {
            if ((object)obj == null) return 1;
            string other = obj as string;
            if ((object)other == null) throw new ArgumentException("Object must be of type String.");
            return CompareOrdinal(this, other);
        }

        public static int Compare(string strA, string strB) { return CompareOrdinal(strA, strB); }

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

        public static string Intern(string str)
        {
            if ((object)str == null) throw new ArgumentNullException("str");
            return InternCore(str);
        }

        public static string IsInterned(string str)
        {
            if ((object)str == null) throw new ArgumentNullException("str");
            return IsInternedCore(str);
        }

        [Lamella.Runtime.RuntimeProvided] private static string InternCore(string str) { return null; }
        [Lamella.Runtime.RuntimeProvided] private static string IsInternedCore(string str) { return null; }

#if LAMELLA_SURFACE_STRING_COMPARISON
        private static bool ComparisonFoldsCase(StringComparison comparisonType)
        {
            switch (comparisonType)
            {
                case StringComparison.CurrentCulture:
                case StringComparison.InvariantCulture:
                case StringComparison.Ordinal:
                    return false;
                case StringComparison.CurrentCultureIgnoreCase:
                case StringComparison.InvariantCultureIgnoreCase:
                case StringComparison.OrdinalIgnoreCase:
                    return true;
                default:
                    throw new ArgumentException("The string comparison type passed in is currently not supported.", "comparisonType");
            }
        }

        private static int NextCodePoint(string s, ref int i, int limit)
        {
            char c = s[i];
            i++;
            if (c >= 0xD800 && c <= 0xDBFF && i < limit)
            {
                char d = s[i];
                if (d >= 0xDC00 && d <= 0xDFFF)
                {
                    i++;
                    return 0x10000 + ((c - 0xD800) << 10) + (d - 0xDC00);
                }
            }
            return c;
        }

        private static int CompareOrdinalIgnoreCase(string a, string b)
        {
            if ((object)a == null) return (object)b == null ? 0 : -1;
            if ((object)b == null) return 1;
            int n = a.Length;
            int m = b.Length;
            int ia = 0;
            int ib = 0;
            while (ia < n && ib < m)
            {
                int ca = NextCodePoint(a, ref ia, n);
                int cb = NextCodePoint(b, ref ib, m);
                if (ca != cb)
                {
                    int fa = CaseMapping.ToUpperCodePoint(ca);
                    int fb = CaseMapping.ToUpperCodePoint(cb);
                    if (fa != fb) return fa - fb;
                }
            }
            return n - m;
        }

        private bool MatchesAtIgnoreCase(string value, int start)
        {
            int m = value.Length;
            int limit = start + m;
            int iv = 0;
            int it = start;
            while (iv < m)
            {
                int cv = NextCodePoint(value, ref iv, m);
                int ct = NextCodePoint(this, ref it, limit);
                if (cv != ct && CaseMapping.ToUpperCodePoint(cv) != CaseMapping.ToUpperCodePoint(ct)) return false;
            }
            return true;
        }

        public static int Compare(string strA, string strB, StringComparison comparisonType)
        {
            if (ComparisonFoldsCase(comparisonType)) return CompareOrdinalIgnoreCase(strA, strB);
            return CompareOrdinal(strA, strB);
        }

        public static bool Equals(string a, string b, StringComparison comparisonType)
        {
            bool foldsCase = ComparisonFoldsCase(comparisonType);
            if ((object)a == (object)b) return true;
            if ((object)a == null || (object)b == null) return false;
            if (a.Length != b.Length) return false;
            if (!foldsCase) return a == b;
            return CompareOrdinalIgnoreCase(a, b) == 0;
        }

        public bool Equals(string value, StringComparison comparisonType)
        {
            return Equals(this, value, comparisonType);
        }

        public bool StartsWith(string value, StringComparison comparisonType)
        {
            if ((object)value == null) throw new ArgumentNullException("value");
            if (!ComparisonFoldsCase(comparisonType)) return StartsWith(value);
            if (value.Length > this.Length) return false;
            return MatchesAtIgnoreCase(value, 0);
        }

        public bool EndsWith(string value, StringComparison comparisonType)
        {
            if ((object)value == null) throw new ArgumentNullException("value");
            if (!ComparisonFoldsCase(comparisonType)) return EndsWith(value);
            if (value.Length > this.Length) return false;
            return MatchesAtIgnoreCase(value, this.Length - value.Length);
        }

        public int IndexOf(string value, StringComparison comparisonType)
        {
            if ((object)value == null) throw new ArgumentNullException("value");
            if (!ComparisonFoldsCase(comparisonType)) return IndexOf(value);
            int last = this.Length - value.Length;
            for (int start = 0; start <= last; start++)
            {
                if (MatchesAtIgnoreCase(value, start)) return start;
            }
            return -1;
        }
#endif

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

        public object Clone() { return this; }
    }
}
