// Lamella managed corlib (from scratch). -- System.Text.StringBuilder
namespace System.Text
{
    public sealed class StringBuilder
    {
        private char[] _chars;
        private int _length;

        private int _maxCapacity;

        public StringBuilder()
        {
            _chars = new char[16];
            _length = 0;
            _maxCapacity = Int32.MaxValue;
        }

        public StringBuilder(int capacity)
        {
            if (capacity < 0) throw new ArgumentOutOfRangeException("capacity");
            _chars = new char[capacity == 0 ? 16 : capacity];
            _length = 0;
            _maxCapacity = Int32.MaxValue;
        }

        public StringBuilder(string value)
        {
            int len = value == null ? 0 : value.Length;
            int cap = len < 16 ? 16 : len;
            _chars = new char[cap];
            for (int i = 0; i < len; i++) _chars[i] = value[i];
            _length = len;
            _maxCapacity = Int32.MaxValue;
        }

        public StringBuilder(int capacity, int maxCapacity)
        {
            if (maxCapacity < 1) throw new ArgumentOutOfRangeException("maxCapacity");
            if (capacity < 0) throw new ArgumentOutOfRangeException("capacity");
            if (capacity > maxCapacity) throw new ArgumentOutOfRangeException("capacity");
            int cap = capacity;
            if (cap == 0) cap = maxCapacity < 16 ? maxCapacity : 16;
            _chars = new char[cap];
            _length = 0;
            _maxCapacity = maxCapacity;
        }

        public StringBuilder(string value, int capacity)
        {
            if (capacity < 0) throw new ArgumentOutOfRangeException("capacity");
            int len = value == null ? 0 : value.Length;
            int cap = capacity < len ? len : capacity;
            if (cap == 0) cap = 16;
            _chars = new char[cap];
            for (int i = 0; i < len; i++) _chars[i] = value[i];
            _length = len;
            _maxCapacity = Int32.MaxValue;
        }

        public StringBuilder(string value, int startIndex, int length, int capacity)
        {
            if (capacity < 0) throw new ArgumentOutOfRangeException("capacity");
            if (startIndex < 0) throw new ArgumentOutOfRangeException("startIndex");
            if (length < 0) throw new ArgumentOutOfRangeException("length");
            int available = value == null ? 0 : value.Length;
            if (startIndex > available - length) throw new ArgumentOutOfRangeException("length");
            int cap = capacity < length ? length : capacity;
            if (cap == 0) cap = 16;
            _chars = new char[cap];
            for (int i = 0; i < length; i++) _chars[i] = value[startIndex + i];
            _length = length;
            _maxCapacity = Int32.MaxValue;
        }

        private void EnsureCapacity(int min)
        {
            if (_chars.Length >= min) return;
            if (min > _maxCapacity) throw new ArgumentOutOfRangeException("capacity");
            int grown = _chars.Length * 2;
            int cap = grown < min ? min : grown;
            if (cap > _maxCapacity) cap = _maxCapacity;
            char[] bigger = new char[cap];
            for (int i = 0; i < _length; i++) bigger[i] = _chars[i];
            _chars = bigger;
        }

        public int Length
        {
            get { return _length; }
            set
            {
                if (value < 0) throw new ArgumentOutOfRangeException("value");
                if (value > _length)
                {
                    EnsureCapacity(value);
                    for (int i = _length; i < value; i++) _chars[i] = '\0';
                }
                _length = value;
            }
        }

        public int Capacity
        {
            get { return _chars.Length; }
            set
            {
                if (value < 0) throw new ArgumentOutOfRangeException("value");
                if (value < _length) throw new ArgumentOutOfRangeException("value");
                if (value > _maxCapacity) throw new ArgumentOutOfRangeException("value");
                if (value == _chars.Length) return;
                char[] resized = new char[value];
                for (int i = 0; i < _length; i++) resized[i] = _chars[i];
                _chars = resized;
            }
        }

        public int MaxCapacity { get { return _maxCapacity; } }

        [System.Runtime.CompilerServices.IndexerName("Chars")]
        public char this[int index]
        {
            get
            {
                if (index < 0 || index >= _length) return _chars[_chars.Length];
                return _chars[index];
            }
            set
            {
                if (index < 0 || index >= _length) throw new ArgumentOutOfRangeException("index");
                _chars[index] = value;
            }
        }

        public StringBuilder Append(string value)
        {
            if (value == null) return this;
            int n = value.Length;
            EnsureCapacity(_length + n);
            for (int i = 0; i < n; i++) _chars[_length + i] = value[i];
            _length += n;
            return this;
        }

        public StringBuilder Append(char value)
        {
            EnsureCapacity(_length + 1);
            _chars[_length] = value;
            _length++;
            return this;
        }

        public StringBuilder Append(int value) { return Append(value.ToString()); }
        public StringBuilder Append(bool value) { return Append(value ? "True" : "False"); }
        public StringBuilder Append(long value) { return Append(value.ToString()); }

        public StringBuilder Append(byte value) { return Append(value.ToString()); }
        public StringBuilder Append(sbyte value) { return Append(value.ToString()); }
        public StringBuilder Append(short value) { return Append(value.ToString()); }
        public StringBuilder Append(ushort value) { return Append(value.ToString()); }
        public StringBuilder Append(uint value) { return Append(value.ToString()); }
        public StringBuilder Append(ulong value) { return Append(value.ToString()); }

#if LAMELLA_SURFACE_FLOAT
        public StringBuilder Append(float value) { return Append(value.ToString()); }
        public StringBuilder Append(double value) { return Append(value.ToString()); }
#endif
        public StringBuilder Append(object value)
        {
            if (value == null) return this;
            return Append(value.ToString());
        }

#if LAMELLA_SURFACE_NETFX_2_0
        public StringBuilder AppendLine() { return Append("\r\n"); }
        public StringBuilder AppendLine(string value) { return Append(value).Append("\r\n"); }
#endif

        public StringBuilder Append(string value, int startIndex, int count)
        {
            if (startIndex < 0) throw new ArgumentOutOfRangeException("startIndex");
            if (count < 0) throw new ArgumentOutOfRangeException("count");
            if (value == null)
            {
                if (count == 0) return this;
                throw new ArgumentNullException("value");
            }
            if (startIndex > value.Length - count) throw new ArgumentOutOfRangeException("startIndex");
            if (count == 0) return this;
            EnsureCapacity(_length + count);
            for (int i = 0; i < count; i++) _chars[_length + i] = value[startIndex + i];
            _length += count;
            return this;
        }

        public StringBuilder Insert(int index, string value)
        {
            if (index < 0 || index > _length) throw new ArgumentOutOfRangeException("index");
            if (value == null) return this;
            int n = value.Length;
            if (n == 0) return this;
            EnsureCapacity(_length + n);
            for (int i = _length - 1; i >= index; i--) _chars[i + n] = _chars[i];
            for (int i = 0; i < n; i++) _chars[index + i] = value[i];
            _length += n;
            return this;
        }

        public StringBuilder Append(char value, int repeatCount)
        {
            if (repeatCount < 0) throw new ArgumentOutOfRangeException("repeatCount");
            EnsureCapacity(_length + repeatCount);
            for (int i = 0; i < repeatCount; i++) _chars[_length + i] = value;
            _length += repeatCount;
            return this;
        }

        public StringBuilder Append(char[] value)
        {
            if (value == null) return this;
            return Append(value, 0, value.Length);
        }

        public StringBuilder Append(char[] value, int startIndex, int charCount)
        {
            if (value == null)
            {
                if (startIndex == 0 && charCount == 0) return this;
                throw new ArgumentNullException("value");
            }
            if (startIndex < 0) throw new ArgumentOutOfRangeException("startIndex");
            if (charCount < 0) throw new ArgumentOutOfRangeException("charCount");
            if (startIndex > value.Length - charCount) throw new ArgumentOutOfRangeException("startIndex");
            EnsureCapacity(_length + charCount);
            for (int i = 0; i < charCount; i++) _chars[_length + i] = value[startIndex + i];
            _length += charCount;
            return this;
        }

        public StringBuilder Insert(int index, char value) { return Insert(index, value.ToString()); }
        public StringBuilder Insert(int index, int value) { return Insert(index, value.ToString()); }
        public StringBuilder Insert(int index, long value) { return Insert(index, value.ToString()); }
        public StringBuilder Insert(int index, bool value) { return Insert(index, value ? "True" : "False"); }
        public StringBuilder Insert(int index, object value)
        {
            return Insert(index, value == null ? "" : value.ToString());
        }

        public StringBuilder Insert(int index, string value, int count)
        {
            if (index < 0 || index > _length) throw new ArgumentOutOfRangeException("index");
            if (count < 0) throw new ArgumentOutOfRangeException("count");
            if (value == null || value.Length == 0 || count == 0) return this;
            int width = value.Length;
            int n = width * count;
            EnsureCapacity(_length + n);
            for (int i = _length - 1; i >= index; i--) _chars[i + n] = _chars[i];
            for (int repeat = 0; repeat < count; repeat++)
            {
                for (int i = 0; i < width; i++) _chars[index + repeat * width + i] = value[i];
            }
            _length += n;
            return this;
        }

        public StringBuilder Insert(int index, char[] value, int startIndex, int charCount)
        {
            if (index < 0 || index > _length) throw new ArgumentOutOfRangeException("index");
            if (startIndex < 0) throw new ArgumentOutOfRangeException("startIndex");
            if (charCount < 0) throw new ArgumentOutOfRangeException("charCount");
            if (value == null)
            {
                if (startIndex == 0 && charCount == 0) return this;
                throw new ArgumentNullException("value");
            }
            if (startIndex > value.Length - charCount) throw new ArgumentOutOfRangeException("startIndex");
            if (charCount == 0) return this;
            EnsureCapacity(_length + charCount);
            for (int i = _length - 1; i >= index; i--) _chars[i + charCount] = _chars[i];
            for (int i = 0; i < charCount; i++) _chars[index + i] = value[startIndex + i];
            _length += charCount;
            return this;
        }

        public StringBuilder Remove(int startIndex, int length)
        {
            if (startIndex < 0) throw new ArgumentOutOfRangeException("startIndex");
            if (length < 0) throw new ArgumentOutOfRangeException("length");
            if (startIndex > _length - length) throw new ArgumentOutOfRangeException("length");
            for (int i = startIndex + length; i < _length; i++) _chars[i - length] = _chars[i];
            _length -= length;
            return this;
        }

        public StringBuilder Replace(char oldChar, char newChar)
        {
            for (int i = 0; i < _length; i++)
            {
                if (_chars[i] == oldChar) _chars[i] = newChar;
            }
            return this;
        }

        public StringBuilder Replace(char oldChar, char newChar, int startIndex, int count)
        {
            if (startIndex < 0) throw new ArgumentOutOfRangeException("startIndex");
            if (count < 0) throw new ArgumentOutOfRangeException("count");
            if (startIndex > _length - count) throw new ArgumentOutOfRangeException("count");
            int limit = startIndex + count;
            for (int i = startIndex; i < limit; i++)
            {
                if (_chars[i] == oldChar) _chars[i] = newChar;
            }
            return this;
        }

        public StringBuilder Replace(string oldValue, string newValue)
        {
            return Replace(oldValue, newValue, 0, _length);
        }

        public StringBuilder Replace(string oldValue, string newValue, int startIndex, int count)
        {
            if (oldValue == null) throw new ArgumentNullException("oldValue");
            if (oldValue.Length == 0) throw new ArgumentException("String cannot be of zero length.", "oldValue");
            if (startIndex < 0) throw new ArgumentOutOfRangeException("startIndex");
            if (count < 0) throw new ArgumentOutOfRangeException("count");
            if (startIndex > _length - count) throw new ArgumentOutOfRangeException("count");
            string replacement = newValue == null ? "" : newValue;
            StringBuilder result = new StringBuilder(_length);
            int limit = startIndex + count;
            int i = 0;
            while (i < _length)
            {
                if (i >= startIndex && i + oldValue.Length <= limit && MatchesAt(i, oldValue))
                {
                    result.Append(replacement);
                    i += oldValue.Length;
                }
                else
                {
                    result.Append(_chars[i]);
                    i = i + 1;
                }
            }
            if (result._length > _maxCapacity) throw new ArgumentOutOfRangeException("capacity");
            _chars = result._chars;
            _length = result._length;
            return this;
        }

        private bool MatchesAt(int at, string value)
        {
            for (int i = 0; i < value.Length; i++)
            {
                if (_chars[at + i] != value[i]) return false;
            }
            return true;
        }

#if LAMELLA_SURFACE_NETFX_4_0
        public StringBuilder Clear()
        {
            _length = 0;
            return this;
        }
#endif

        public override string ToString()
        {
            return String.CreateFromChars(_chars, 0, _length);
        }

        public string ToString(int startIndex, int length)
        {
            if (startIndex < 0) throw new ArgumentOutOfRangeException("startIndex");
            if (length < 0) throw new ArgumentOutOfRangeException("length");
            if (startIndex > _length - length) throw new ArgumentOutOfRangeException("length");
            return String.CreateFromChars(_chars, startIndex, length);
        }
    }
}
