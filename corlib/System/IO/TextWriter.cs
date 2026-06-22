// Lamella managed corlib (from scratch). -- System.IO.TextWriter
namespace System.IO
{
    public abstract class TextWriter : IDisposable
    {
        private string _coreNewLine = "\n";
        public virtual string NewLine
        {
            get { return _coreNewLine; }
            set { _coreNewLine = (object)value == null ? "\n" : value; }
        }

        public abstract void Write(char value);

        public virtual void Write(string value)
        {
            if (value == null) return;
            for (int i = 0; i < value.Length; i++)
            {
                Write(value[i]);
            }
        }

        public virtual void WriteLine()
        {
            Write(NewLine);
        }

        public virtual void WriteLine(string value)
        {
            Write(value);
            Write(NewLine);
        }

        public virtual void WriteLine(char value)
        {
            Write(value);
            Write(NewLine);
        }

        public virtual void Flush()
        {
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
