// Lamella managed corlib (from scratch). -- System.Text.StringBuilder
namespace System.Text
{
    public sealed class StringBuilder
    {
        private char[] _chars;
        private int _length;

        public StringBuilder()
        {
            _chars = new char[16];
            _length = 0;
        }

        public StringBuilder(int capacity)
        {
            if (capacity < 0) throw new ArgumentOutOfRangeException("capacity");
            _chars = new char[capacity == 0 ? 16 : capacity];
            _length = 0;
        }

        public StringBuilder(string value)
        {
            int len = value == null ? 0 : value.Length;
            int cap = len < 16 ? 16 : len;
            _chars = new char[cap];
            for (int i = 0; i < len; i++) _chars[i] = value[i];
            _length = len;
        }

        private void EnsureCapacity(int min)
        {
            if (_chars.Length >= min) return;
            int grown = _chars.Length * 2;
            int cap = grown < min ? min : grown;
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

        public int Capacity { get { return _chars.Length; } }

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
        public StringBuilder Append(object value)
        {
            if (value == null) return this;
            return Append(value.ToString());
        }

        public StringBuilder AppendLine() { return Append("\r\n"); }
        public StringBuilder AppendLine(string value) { return Append(value).Append("\r\n"); }

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

        public StringBuilder Clear()
        {
            _length = 0;
            return this;
        }

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
