// Lamella managed corlib (from scratch). -- System.IO.BinaryWriter
namespace System.IO
{
    public class BinaryWriter : IDisposable
    {
        private Stream _stream;
        private System.Text.Encoding _encoding;

        public BinaryWriter(Stream output)
        {
            if ((object)output == null) throw new ArgumentNullException("output");
            if (!output.CanWrite) throw new ArgumentException("Stream was not writable.");
            _stream = output;
            _encoding = System.Text.Encoding.UTF8;
        }

        public BinaryWriter(Stream output, System.Text.Encoding encoding)
        {
            if ((object)output == null) throw new ArgumentNullException("output");
            if ((object)encoding == null) throw new ArgumentNullException("encoding");
            if (!output.CanWrite) throw new ArgumentException("Stream was not writable.");
            _stream = output;
            _encoding = encoding;
        }

        public virtual Stream BaseStream
        {
            get
            {
                _stream.Flush();
                return _stream;
            }
        }

        public virtual void Write(bool value)
        {
            _stream.WriteByte((byte)(value ? 1 : 0));
        }

        public virtual void Write(byte value)
        {
            _stream.WriteByte(value);
        }

        public virtual void Write(sbyte value)
        {
            _stream.WriteByte((byte)value);
        }

        public virtual void Write(char value)
        {
            System.Text.StringBuilder one = new System.Text.StringBuilder();
            one.Append(value);
            byte[] encoded = _encoding.GetBytes(one.ToString());
            _stream.Write(encoded, 0, encoded.Length);
        }

        public virtual void Write(short value)
        {
            byte[] bytes = BitConverter.GetBytes(value);
            _stream.Write(bytes, 0, 2);
        }

        public virtual void Write(ushort value)
        {
            byte[] bytes = BitConverter.GetBytes(value);
            _stream.Write(bytes, 0, 2);
        }

        public virtual void Write(int value)
        {
            byte[] bytes = BitConverter.GetBytes(value);
            _stream.Write(bytes, 0, 4);
        }

        public virtual void Write(uint value)
        {
            byte[] bytes = BitConverter.GetBytes(value);
            _stream.Write(bytes, 0, 4);
        }

        public virtual void Write(long value)
        {
            byte[] bytes = BitConverter.GetBytes(value);
            _stream.Write(bytes, 0, 8);
        }

        public virtual void Write(ulong value)
        {
            byte[] bytes = BitConverter.GetBytes(value);
            _stream.Write(bytes, 0, 8);
        }

        public virtual void Write(float value)
        {
            byte[] bytes = BitConverter.GetBytes(value);
            _stream.Write(bytes, 0, 4);
        }

        public virtual void Write(double value)
        {
            byte[] bytes = BitConverter.GetBytes(value);
            _stream.Write(bytes, 0, 8);
        }

        public virtual void Write(string value)
        {
            if ((object)value == null) throw new ArgumentNullException("value");
            byte[] bytes = _encoding.GetBytes(value);
            Write7BitEncodedInt(bytes.Length);
            _stream.Write(bytes, 0, bytes.Length);
        }

        public virtual void Write(byte[] buffer)
        {
            if ((object)buffer == null) throw new ArgumentNullException("buffer");
            _stream.Write(buffer, 0, buffer.Length);
        }

        public virtual void Write(byte[] buffer, int index, int count)
        {
            if ((object)buffer == null) throw new ArgumentNullException("buffer");
            _stream.Write(buffer, index, count);
        }

        public virtual void Write(char[] chars)
        {
            if ((object)chars == null) throw new ArgumentNullException("chars");
            System.Text.StringBuilder sb = new System.Text.StringBuilder();
            for (int i = 0; i < chars.Length; i++) sb.Append(chars[i]);
            byte[] encoded = _encoding.GetBytes(sb.ToString());
            _stream.Write(encoded, 0, encoded.Length);
        }

        protected void Write7BitEncodedInt(int value)
        {
            uint v = (uint)value;
            while (v >= 0x80)
            {
                _stream.WriteByte((byte)(v | 0x80));
                v = v >> 7;
            }
            _stream.WriteByte((byte)v);
        }

        public virtual long Seek(int offset, SeekOrigin origin)
        {
            return _stream.Seek(offset, origin);
        }

        public virtual void Flush()
        {
            _stream.Flush();
        }

        public virtual void Close()
        {
            Dispose(true);
        }

        public void Dispose()
        {
            Dispose(true);
        }

        protected virtual void Dispose(bool disposing)
        {
            if (disposing)
            {
                _stream.Close();
            }
        }
    }
}
