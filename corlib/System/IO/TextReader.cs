// Lamella managed corlib (from scratch). -- System.IO.TextReader
namespace System.IO
{
    public abstract class TextReader : IDisposable
    {
        public abstract int Peek();

        public abstract int Read();

        public virtual int Read(char[] buffer, int index, int count)
        {
            if ((object)buffer == null) throw new ArgumentNullException("buffer");
            if (index < 0) throw new ArgumentOutOfRangeException("index");
            if (count < 0) throw new ArgumentOutOfRangeException("count");
            if (buffer.Length - index < count) throw new ArgumentException("Offset and length were out of bounds for the array or count is greater than the number of elements from index to the end of the source collection.");

            int read = 0;
            while (read < count)
            {
                int c = Read();
                if (c == -1) break;
                buffer[index + read] = (char)c;
                read = read + 1;
            }
            return read;
        }

        public virtual string ReadLine()
        {
            System.Text.StringBuilder sb = null;
            while (true)
            {
                int c = Read();
                if (c == -1)
                {
                    if (sb == null) return null;
                    return sb.ToString();
                }
                if (c == '\r' || c == '\n')
                {
                    if (c == '\r' && Peek() == '\n') Read();
                    if (sb == null) return "";
                    return sb.ToString();
                }
                if (sb == null) sb = new System.Text.StringBuilder();
                sb.Append((char)c);
            }
        }

        public virtual string ReadToEnd()
        {
            System.Text.StringBuilder sb = new System.Text.StringBuilder();
            int c = Read();
            while (c != -1)
            {
                sb.Append((char)c);
                c = Read();
            }
            return sb.ToString();
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
        }
    }
}
