// Lamella managed corlib (from scratch). -- System.Text.StringBuilder
namespace System.Text
{
    public sealed class StringBuilder
    {
        [Lamella.Runtime.RuntimeProvided] public StringBuilder() { }
        [Lamella.Runtime.RuntimeProvided] public StringBuilder(int capacity) { }
        [Lamella.Runtime.RuntimeProvided] public StringBuilder(string value) { }

        public int Length
        {
            [Lamella.Runtime.RuntimeProvided] get { return 0; }
            set
            {
                if (value < 0) throw new ArgumentOutOfRangeException("value");
                SetLengthCore(value);
            }
        }

        [Lamella.Runtime.RuntimeProvided] private void SetLengthCore(int value) { }

        public int Capacity { [Lamella.Runtime.RuntimeProvided] get { return 0; } }

        [System.Runtime.CompilerServices.IndexerName("Chars")]
        public char this[int index]
        {
            [Lamella.Runtime.RuntimeProvided] get { return '\0'; }
            set
            {
                if (index < 0 || index >= Length) throw new ArgumentOutOfRangeException("index");
                SetCharsCore(index, value);
            }
        }

        [Lamella.Runtime.RuntimeProvided] private void SetCharsCore(int index, char value) { }

        [Lamella.Runtime.RuntimeProvided] public StringBuilder Append(string value) { return null; }
        [Lamella.Runtime.RuntimeProvided] public StringBuilder Append(char value) { return null; }
        [Lamella.Runtime.RuntimeProvided] public StringBuilder Append(int value) { return null; }
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
            if (index < 0 || index > Length) throw new ArgumentOutOfRangeException("index");
            if (value == null) return this;
            InsertCore(index, value);
            return this;
        }

        [Lamella.Runtime.RuntimeProvided] private void InsertCore(int index, string value) { }

        public StringBuilder Remove(int startIndex, int length)
        {
            if (startIndex < 0) throw new ArgumentOutOfRangeException("startIndex");
            if (length < 0) throw new ArgumentOutOfRangeException("length");
            if (startIndex > Length - length) throw new ArgumentOutOfRangeException("length");
            RemoveCore(startIndex, length);
            return this;
        }

        [Lamella.Runtime.RuntimeProvided] private void RemoveCore(int startIndex, int length) { }

        [Lamella.Runtime.RuntimeProvided] public StringBuilder Replace(char oldChar, char newChar) { return null; }

        public StringBuilder Clear()
        {
            Length = 0;
            return this;
        }

        [Lamella.Runtime.RuntimeProvided] public override string ToString() { return null; }

        public string ToString(int startIndex, int length)
        {
            return ToString().Substring(startIndex, length);
        }
    }
}
