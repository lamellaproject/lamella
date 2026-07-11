// Lamella managed corlib (from scratch). -- System.IO.StringReader
namespace System.IO
{
    public class StringReader : TextReader
    {
        private string _text;
        private int _position;

        public StringReader(string s)
        {
            if ((object)s == null) throw new ArgumentNullException("s");
            _text = s;
        }

        public override int Peek()
        {
            EnsureOpen();
            if (_position >= _text.Length) return -1;
            return _text[_position];
        }

        public override int Read()
        {
            EnsureOpen();
            if (_position >= _text.Length) return -1;
            char value = _text[_position];
            _position++;
            return value;
        }

        public override int Read(char[] buffer, int index, int count)
        {
            if ((object)buffer == null) throw new ArgumentNullException("buffer");
            if (index < 0) throw new ArgumentOutOfRangeException("index");
            if (count < 0) throw new ArgumentOutOfRangeException("count");
            if (buffer.Length - index < count)
                throw new ArgumentException("Offset and length were out of bounds for the array or count is greater than the number of elements from index to the end of the source collection.");
            EnsureOpen();
            int available = _text.Length - _position;
            int copied = count < available ? count : available;
            for (int i = 0; i < copied; i++)
                buffer[index + i] = _text[_position + i];
            _position += copied;
            return copied;
        }

        public override string ReadToEnd()
        {
            EnsureOpen();
            string rest = _text.Substring(_position);
            _position = _text.Length;
            return rest;
        }

        public override void Close()
        {
            _text = null;
        }

        private void EnsureOpen()
        {
            if ((object)_text == null) throw new ObjectDisposedException("StringReader");
        }
    }
}
