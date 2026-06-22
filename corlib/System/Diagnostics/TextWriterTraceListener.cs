// Lamella managed corlib (from scratch). -- System.Diagnostics.TextWriterTraceListener
namespace System.Diagnostics
{
    public class TextWriterTraceListener : TraceListener
    {
        private System.IO.TextWriter _writer;

        public TextWriterTraceListener()
        {
            _writer = null;
        }

        public TextWriterTraceListener(System.IO.TextWriter writer)
        {
            _writer = writer;
        }

        public TextWriterTraceListener(System.IO.TextWriter writer, string name) : base(name)
        {
            _writer = writer;
        }

        public System.IO.TextWriter Writer
        {
            get { return _writer; }
            set { _writer = value; }
        }

        public override void Write(string message)
        {
            if (_writer == null) return;
            if (NeedIndent) WriteIndent();
            _writer.Write(message);
        }

        public override void WriteLine(string message)
        {
            if (_writer == null) return;
            if (NeedIndent) WriteIndent();
            _writer.WriteLine(message);
            NeedIndent = true;
        }

        public override void Flush()
        {
            if (_writer != null) _writer.Flush();
        }

        public override void Close()
        {
            if (_writer != null) _writer.Close();
        }
    }
}
