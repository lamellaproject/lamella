// Lamella managed corlib (from scratch). -- System.Diagnostics.TraceListener
namespace System.Diagnostics
{
    public abstract class TraceListener
    {
        private string _name;
        private int _indentLevel;
        private int _indentSize;

        protected bool NeedIndent;

        protected TraceListener()
        {
            _name = "";
            _indentLevel = 0;
            _indentSize = 4;
            NeedIndent = true;
        }

        protected TraceListener(string name)
        {
            _name = name == null ? "" : name;
            _indentLevel = 0;
            _indentSize = 4;
            NeedIndent = true;
        }

        public virtual string Name
        {
            get { return _name; }
            set { _name = value == null ? "" : value; }
        }

        public int IndentLevel
        {
            get { return _indentLevel; }
            set { _indentLevel = value < 0 ? 0 : value; }
        }

        public int IndentSize
        {
            get { return _indentSize; }
            set { _indentSize = value < 0 ? 0 : value; }
        }

        public abstract void Write(string message);
        public abstract void WriteLine(string message);

        protected virtual void WriteIndent()
        {
            NeedIndent = false;
            int spaces = _indentLevel * _indentSize;
            for (int i = 0; i < spaces; i++)
            {
                Write(" ");
            }
        }

        public virtual void Flush()
        {
        }

        public virtual void Close()
        {
        }

        public virtual void Fail(string message)
        {
            Fail(message, "");
        }

        public virtual void Fail(string message, string detailMessage)
        {
            string m = message == null ? "" : message;
            string d = detailMessage == null ? "" : detailMessage;
            WriteLine("Fail: " + m + " " + d);
        }
    }
}
