// Lamella managed corlib (from scratch). 
namespace System
{
    public class Object
    {
        public Object() { }
        public virtual bool Equals(object o) { return (object)this == o; }
        public virtual int GetHashCode() { return 0; }
        public virtual string ToString() { return null; }
    }
    public struct Void { }
    public abstract class ValueType : Object { }
    public abstract class Enum : ValueType { }
    public struct Boolean { }
    public struct Char
    {
        // Managed classification, ASCII-correct (matches .NET across the ASCII range; from scratch).
        public static bool IsDigit(char c) { return c >= '0' && c <= '9'; }
        public static bool IsLetter(char c) { return (c >= 'A' && c <= 'Z') || (c >= 'a' && c <= 'z'); }
        public static bool IsWhiteSpace(char c) { return c == ' ' || c == '\t' || c == '\n' || c == '\r'; }

        // ASCII case shift (managed): invariant for letters, identity otherwise. Matches .NET for ASCII.
        public static char ToUpper(char c) { if (c >= 'a' && c <= 'z') return (char)(c - 32); return c; }
        public static char ToLower(char c) { if (c >= 'A' && c <= 'Z') return (char)(c + 32); return c; }
    }
    public struct SByte { }
    public struct Byte { }
    public struct Int16 { }
    public struct UInt16 { }
    public struct Int32
    {
        // Managed base-10 parse (from scratch): optional sign then digits. Matches .NET for valid
        // input; the full Parse throws FormatException/OverflowException, not modeled here.
        public static int Parse(string s)
        {
            int result = 0;
            int i = 0;
            bool negative = false;
            if (s[0] == '-') { negative = true; i = 1; }
            else if (s[0] == '+') { i = 1; }
            while (i < s.Length)
            {
                result = result * 10 + (s[i] - '0');
                i = i + 1;
            }
            return negative ? -result : result;
        }
    }
    public struct UInt32 { }
    public struct Int64 { }
    public struct UInt64 { }
    public struct Single { }
    public struct Double { }
    public struct IntPtr { }
    public struct UIntPtr { }
    public sealed class String
    {
        // The character data lives in the VES heap; these are the runtime-provided readers.
        public int Length { [Lamella.Runtime.RuntimeProvided] get { return 0; } }
        [System.Runtime.CompilerServices.IndexerName("Chars")]
        public char this[int index] { [Lamella.Runtime.RuntimeProvided] get { return '\0'; } }

        // Substring allocates a new string from a code-unit range -- a runtime-provided allocation
        // primitive (like the indexer reader); the VES supplies it (string_substring / _len).
        [Lamella.Runtime.RuntimeProvided] public string Substring(int startIndex) { return null; }
        [Lamella.Runtime.RuntimeProvided] public string Substring(int startIndex, int length) { return null; }

        // Ordinal value equality, written from scratch (ECMA-334: string == is ordinal). The
        // reference fast-path also covers the both-null case.
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

        // First index of an exact code-unit match, or -1 (ordinal).
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

        // Ordinal prefix/suffix tests (managed). Matches .NET for the common case-sensitive ASCII use.
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

        // Managed: copy each code unit into a fresh char[] (newarr, no allocation primitive needed).
        public char[] ToCharArray()
        {
            char[] result = new char[this.Length];
            for (int i = 0; i < result.Length; i++) result[i] = this[i];
            return result;
        }

        // Managed: trim ASCII whitespace by composing Char.IsWhiteSpace + Substring (both corlib).
        // Matches .NET for ASCII whitespace.
        public string Trim()
        {
            int start = 0;
            int end = this.Length - 1;
            while (start <= end && Char.IsWhiteSpace(this[start])) start++;
            while (end >= start && Char.IsWhiteSpace(this[end])) end--;
            return this.Substring(start, end - start + 1);
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
    public abstract class Array
    {
        public int Length { [Lamella.Runtime.RuntimeProvided] get { return 0; } }
        [Lamella.Runtime.RuntimeProvided] public object GetValue(int index) { return null; }
        [Lamella.Runtime.RuntimeProvided] public void SetValue(object value, int index) { }

        // Managed, from scratch: in-place reversal via the untyped element primitives.
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
    }
    public class Exception { public Exception() { } public Exception(string message) { } }
    public struct RuntimeTypeHandle { }
    public struct RuntimeFieldHandle { }
    public struct RuntimeMethodHandle { }
    public class Type { }
    public abstract class Delegate { }
    public abstract class MulticastDelegate : Delegate { }

    public enum AttributeTargets { All = 32767 }
    public class Attribute { public Attribute() { } }
    public sealed class AttributeUsageAttribute : Attribute
    {
        private bool _allowMultiple;
        private bool _inherited;
        public AttributeUsageAttribute(AttributeTargets validOn) { }
        public bool AllowMultiple { get { return _allowMultiple; } set { _allowMultiple = value; } }
        public bool Inherited { get { return _inherited; } set { _inherited = value; } }
    }
    public sealed class ParamArrayAttribute : Attribute { public ParamArrayAttribute() { } }

    // System.Console: output primitives the VES supplies (the build pass makes them `runtime`).
    public sealed class Console
    {
        [Lamella.Runtime.RuntimeProvided] public static void WriteLine(int value) { }
        [Lamella.Runtime.RuntimeProvided] public static void WriteLine(string value) { }
        [Lamella.Runtime.RuntimeProvided] public static void WriteLine(bool value) { }
        [Lamella.Runtime.RuntimeProvided] public static void WriteLine(char value) { }
        [Lamella.Runtime.RuntimeProvided] public static void WriteLine(long value) { }
    }

    // System.Math: managed C# over the primitives/operators -- ordinary methods the interpreter
    // runs as CIL (no runtime primitive needed for integer min/max/abs).
    public sealed class Math
    {
        public static int Max(int a, int b) { return a >= b ? a : b; }
        public static int Min(int a, int b) { return a <= b ? a : b; }
        public static int Abs(int value) { return value < 0 ? -value : value; }
        public static int Sign(int value) { return value > 0 ? 1 : (value < 0 ? -1 : 0); }
        public static long Max(long a, long b) { return a >= b ? a : b; }
        public static long Min(long a, long b) { return a <= b ? a : b; }
        public static long Abs(long value) { return value < 0 ? -value : value; }
    }

    // System.Convert: managed conversions that compose the primitives' own parse/format.
    public sealed class Convert
    {
        public static int ToInt32(string value) { return Int32.Parse(value); }
        public static string ToString(bool value) { return value ? "True" : "False"; }
    }
}

namespace System.Collections
{
    // A managed dynamic array (from scratch), backed by an object[] that grows by doubling. No
    // runtime primitives -- it is ordinary C# over object[] (newarr / ldelem.ref / stelem.ref /
    // ldlen), the interpreter executing every method as CIL. Overrides the native intrinsic
    // ArrayList because the corlib resolves ahead of it.
    public class ArrayList
    {
        private object[] items;
        private int size;

        public ArrayList() { items = new object[4]; size = 0; }

        public int Count { get { return size; } }

        public object this[int index]
        {
            get { return items[index]; }
            set { items[index] = value; }
        }

        public int Add(object value)
        {
            if (size == items.Length)
            {
                object[] bigger = new object[items.Length * 2];
                for (int i = 0; i < size; i++) bigger[i] = items[i];
                items = bigger;
            }
            items[size] = value;
            size = size + 1;
            return size - 1;
        }
    }

    // A LIFO stack (from scratch), object[]-backed, growing by doubling.
    public class Stack
    {
        private object[] items;
        private int size;
        public Stack() { items = new object[4]; size = 0; }
        public int Count { get { return size; } }
        public void Push(object value)
        {
            if (size == items.Length)
            {
                object[] bigger = new object[items.Length * 2];
                for (int i = 0; i < size; i++) bigger[i] = items[i];
                items = bigger;
            }
            items[size] = value;
            size = size + 1;
        }
        public object Pop()
        {
            size = size - 1;
            object value = items[size];
            items[size] = null;
            return value;
        }
        public object Peek() { return items[size - 1]; }
    }

    // A FIFO queue (from scratch), object[]-backed; Dequeue shifts the front off (simple, O(n)).
    public class Queue
    {
        private object[] items;
        private int size;
        public Queue() { items = new object[4]; size = 0; }
        public int Count { get { return size; } }
        public void Enqueue(object value)
        {
            if (size == items.Length)
            {
                object[] bigger = new object[items.Length * 2];
                for (int i = 0; i < size; i++) bigger[i] = items[i];
                items = bigger;
            }
            items[size] = value;
            size = size + 1;
        }
        public object Dequeue()
        {
            object value = items[0];
            for (int i = 1; i < size; i++) items[i - 1] = items[i];
            size = size - 1;
            items[size] = null;
            return value;
        }
        public object Peek() { return items[0]; }
    }

    // A managed key/value map (from scratch), parallel object[] keys + values. Keys compared by
    // value via Object.Equals (so a computed string key matches an equal stored key) -- a linear
    // scan (no hashing yet; GetHashCode is unused here). Overrides the native intrinsic Hashtable.
    public class Hashtable
    {
        private object[] keys;
        private object[] values;
        private int size;

        public Hashtable()
        {
            keys = new object[4];
            values = new object[4];
            size = 0;
        }

        public int Count { get { return size; } }

        private int IndexOfKey(object key)
        {
            for (int i = 0; i < size; i++)
            {
                if (keys[i].Equals(key)) return i;
            }
            return -1;
        }

        public bool Contains(object key) { return IndexOfKey(key) >= 0; }
        public bool ContainsKey(object key) { return IndexOfKey(key) >= 0; }

        public object this[object key]
        {
            get
            {
                int i = IndexOfKey(key);
                if (i < 0) return null;
                return values[i];
            }
            set
            {
                int i = IndexOfKey(key);
                if (i >= 0) { values[i] = value; return; }
                if (size == keys.Length)
                {
                    object[] bk = new object[keys.Length * 2];
                    object[] bv = new object[values.Length * 2];
                    for (int j = 0; j < size; j++) { bk[j] = keys[j]; bv[j] = values[j]; }
                    keys = bk;
                    values = bv;
                }
                keys[size] = key;
                values[size] = value;
                size = size + 1;
            }
        }

        public void Add(object key, object value) { this[key] = value; }
    }

    // A managed bit vector (from scratch), packed into an int[] (32 bits/word). No runtime primitives
    // -- ordinary C# bit ops the interpreter runs as CIL. Overrides the native intrinsic if any.
    public class BitArray
    {
        private int[] bits;
        private int length;

        public BitArray(int length)
        {
            this.length = length;
            bits = new int[(length + 31) / 32];
        }

        public int Length { get { return length; } }
        public int Count { get { return length; } }

        public bool Get(int index)
        {
            return (bits[index / 32] & (1 << (index % 32))) != 0;
        }

        public void Set(int index, bool value)
        {
            int word = index / 32;
            int mask = 1 << (index % 32);
            if (value) bits[word] = bits[word] | mask;
            else bits[word] = bits[word] & ~mask;
        }
    }
}
namespace System.Reflection
{
    // csc marks any type with an indexer `[DefaultMember]`; /nostdlib needs the type to exist.
    public sealed class DefaultMemberAttribute : System.Attribute
    {
        public DefaultMemberAttribute(string memberName) { }
    }
}
namespace System.Runtime.CompilerServices
{
    // Names the String indexer accessor `get_Chars` (matching .NET + the intrinsic registry),
    // rather than the default `get_Item`.
    public sealed class IndexerNameAttribute : System.Attribute
    {
        public IndexerNameAttribute(string indexerName) { }
    }
}
namespace Lamella.Runtime
{
    // Marks a method whose body the VES provides. The build pass lowers each marked method to
    // ECMA-335 `runtime managed`. Deliberately our own marker, NOT System...InternalCall (which
    // ECMA-335 II.23.1.11 reserves as non-conforming).
    public sealed class RuntimeProvidedAttribute : System.Attribute
    {
        public RuntimeProvidedAttribute() { }
    }
}
