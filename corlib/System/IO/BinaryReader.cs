// Lamella managed corlib (from scratch). -- System.IO.BinaryReader
namespace System.IO
{
    public class BinaryReader : IDisposable
    {
        private Stream _stream;
        private System.Text.Encoding _encoding;

        public BinaryReader(Stream input)
        {
            if ((object)input == null) throw new ArgumentNullException("input");
            if (!input.CanRead) throw new ArgumentException("Stream was not readable.");
            _stream = input;
            _encoding = System.Text.Encoding.UTF8;
        }

        public BinaryReader(Stream input, System.Text.Encoding encoding)
        {
            if ((object)input == null) throw new ArgumentNullException("input");
            if ((object)encoding == null) throw new ArgumentNullException("encoding");
            if (!input.CanRead) throw new ArgumentException("Stream was not readable.");
            _stream = input;
            _encoding = encoding;
        }

        public virtual Stream BaseStream
        {
            get { return _stream; }
        }

        private byte[] ReadExact(int count)
        {
            byte[] buffer = new byte[count];
            int filled = 0;
            while (filled < count)
            {
                int read = _stream.Read(buffer, filled, count - filled);
                if (read == 0) throw new EndOfStreamException("Unable to read beyond the end of the stream.");
                filled = filled + read;
            }
            return buffer;
        }

        public virtual bool ReadBoolean()
        {
            int b = _stream.ReadByte();
            if (b < 0) throw new EndOfStreamException("Unable to read beyond the end of the stream.");
            return b != 0;
        }

        public virtual byte ReadByte()
        {
            int b = _stream.ReadByte();
            if (b < 0) throw new EndOfStreamException("Unable to read beyond the end of the stream.");
            return (byte)b;
        }

        public virtual sbyte ReadSByte()
        {
            return (sbyte)ReadByte();
        }

        public virtual short ReadInt16()
        {
            byte[] bytes = ReadExact(2);
            return BitConverter.ToInt16(bytes, 0);
        }

        public virtual ushort ReadUInt16()
        {
            byte[] bytes = ReadExact(2);
            return BitConverter.ToUInt16(bytes, 0);
        }

        public virtual int ReadInt32()
        {
            byte[] bytes = ReadExact(4);
            return BitConverter.ToInt32(bytes, 0);
        }

        public virtual uint ReadUInt32()
        {
            byte[] bytes = ReadExact(4);
            return BitConverter.ToUInt32(bytes, 0);
        }

        public virtual long ReadInt64()
        {
            byte[] bytes = ReadExact(8);
            return BitConverter.ToInt64(bytes, 0);
        }

        public virtual ulong ReadUInt64()
        {
            byte[] bytes = ReadExact(8);
            return BitConverter.ToUInt64(bytes, 0);
        }

        public virtual float ReadSingle()
        {
            byte[] bytes = ReadExact(4);
            return BitConverter.ToSingle(bytes, 0);
        }

        public virtual double ReadDouble()
        {
            byte[] bytes = ReadExact(8);
            return BitConverter.ToDouble(bytes, 0);
        }

        private static int Utf8SequenceLength(int lead)
        {
            if (lead < 0x80) return 1;
            if (lead < 0xE0) return 2;
            if (lead < 0xF0) return 3;
            return 4;
        }

        public virtual char ReadChar()
        {
            int lead = _stream.ReadByte();
            if (lead < 0) throw new EndOfStreamException("Unable to read beyond the end of the stream.");
            int length = Utf8SequenceLength(lead);
            byte[] sequence = new byte[length];
            sequence[0] = (byte)lead;
            for (int i = 1; i < length; i++)
            {
                int next = _stream.ReadByte();
                if (next < 0) throw new EndOfStreamException("Unable to read beyond the end of the stream.");
                sequence[i] = (byte)next;
            }
            string decoded = _encoding.GetString(sequence);
            return decoded[0];
        }

        public virtual char[] ReadChars(int count)
        {
            if (count < 0) throw new ArgumentOutOfRangeException("count");
            System.Text.StringBuilder sb = new System.Text.StringBuilder();
            int produced = 0;
            while (produced < count)
            {
                sb.Append(ReadChar());
                produced = produced + 1;
            }
            string text = sb.ToString();
            char[] result = new char[text.Length];
            for (int i = 0; i < text.Length; i++) result[i] = text[i];
            return result;
        }

        public virtual string ReadString()
        {
            int byteCount = Read7BitEncodedInt();
            if (byteCount < 0) throw new IOException("Binary stream contains an invalid string length.");
            if (byteCount == 0) return "";
            byte[] bytes = ReadExact(byteCount);
            return _encoding.GetString(bytes);
        }

        public virtual byte[] ReadBytes(int count)
        {
            if (count < 0) throw new ArgumentOutOfRangeException("count");
            byte[] buffer = new byte[count];
            int filled = 0;
            while (filled < count)
            {
                int read = _stream.Read(buffer, filled, count - filled);
                if (read == 0) break;
                filled = filled + read;
            }
            if (filled == count) return buffer;
            byte[] trimmed = new byte[filled];
            if (filled > 0) Buffer.BlockCopy(buffer, 0, trimmed, 0, filled);
            return trimmed;
        }

        protected int Read7BitEncodedInt()
        {
            int result = 0;
            int shift = 0;
            while (shift != 35)
            {
                int b = _stream.ReadByte();
                if (b < 0) throw new EndOfStreamException("Unable to read beyond the end of the stream.");
                result = result | ((b & 0x7F) << shift);
                shift = shift + 7;
                if ((b & 0x80) == 0) return result;
            }
            throw new FormatException("Too many bytes in what should have been a 7-bit encoded integer.");
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
