// Lamella managed corlib (from scratch). -- System.IO.StringWriter
namespace System.IO
{
    public class StringWriter : TextWriter
    {
        private System.Text.StringBuilder _builder;
        private string _newLine = "\r\n";

        public StringWriter() : this(new System.Text.StringBuilder()) { }

        public StringWriter(System.Text.StringBuilder sb)
        {
            if ((object)sb == null) throw new ArgumentNullException("sb");
            _builder = sb;
        }

        public override string NewLine
        {
            get { return _newLine; }
            set { _newLine = (object)value == null ? "\r\n" : value; }
        }

        public override void Write(char value)
        {
            EnsureOpen();
            _builder.Append(value);
        }

        public override void Write(string value)
        {
            EnsureOpen();
            if ((object)value != null) _builder.Append(value);
        }

        public virtual System.Text.StringBuilder GetStringBuilder()
        {
            return _builder;
        }

        public override string ToString()
        {
            return _builder.ToString();
        }

        public override void Close()
        {
            _open = false;
        }

        private bool _open = true;

        private void EnsureOpen()
        {
            if (!_open) throw new ObjectDisposedException("StringWriter");
        }
    }
}
