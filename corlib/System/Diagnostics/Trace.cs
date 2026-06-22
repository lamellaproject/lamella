// Lamella managed corlib (from scratch). -- System.Diagnostics.Trace
namespace System.Diagnostics
{
    public sealed class Trace
    {
        private static TraceListenerCollection _listeners;
        private static int _indentLevel;
        private static int _indentSize = 4;

        public static TraceListenerCollection Listeners
        {
            get
            {
                if ((object)_listeners == null)
                {
                    _listeners = new TraceListenerCollection();
                    _listeners.Add(new DefaultTraceListener());
                }
                return _listeners;
            }
        }

        public static int IndentLevel
        {
            get { return _indentLevel; }
            set
            {
                _indentLevel = value < 0 ? 0 : value;
                PropagateIndent();
            }
        }

        public static int IndentSize
        {
            get { return _indentSize; }
            set
            {
                _indentSize = value < 0 ? 0 : value;
                PropagateIndent();
            }
        }

        private static void PropagateIndent()
        {
            TraceListenerCollection ls = Listeners;
            for (int i = 0; i < ls.Count; i++)
            {
                TraceListener l = ls[i];
                if (l != null)
                {
                    l.IndentLevel = _indentLevel;
                    l.IndentSize = _indentSize;
                }
            }
        }

        public static void Indent()
        {
            IndentLevel = _indentLevel + 1;
        }

        public static void Unindent()
        {
            IndentLevel = _indentLevel - 1;
        }

        public static void Write(string message)
        {
            PropagateIndent();
            TraceListenerCollection ls = Listeners;
            for (int i = 0; i < ls.Count; i++)
            {
                TraceListener l = ls[i];
                if (l != null) l.Write(message);
            }
        }

        public static void Write(object value)
        {
            Write(value == null ? "" : value.ToString());
        }

        public static void WriteLine(string message)
        {
            PropagateIndent();
            TraceListenerCollection ls = Listeners;
            for (int i = 0; i < ls.Count; i++)
            {
                TraceListener l = ls[i];
                if (l != null) l.WriteLine(message);
            }
        }

        public static void WriteLine(object value)
        {
            WriteLine(value == null ? "" : value.ToString());
        }

        public static void Print(string message)
        {
            WriteLine(message);
        }

        public static void WriteIf(bool condition, string message)
        {
            if (condition) Write(message);
        }

        public static void WriteIf(bool condition, object value)
        {
            if (condition) Write(value);
        }

        public static void WriteLineIf(bool condition, string message)
        {
            if (condition) WriteLine(message);
        }

        public static void WriteLineIf(bool condition, object value)
        {
            if (condition) WriteLine(value);
        }

        public static void Flush()
        {
            TraceListenerCollection ls = Listeners;
            for (int i = 0; i < ls.Count; i++)
            {
                TraceListener l = ls[i];
                if (l != null) l.Flush();
            }
        }

        public static void Close()
        {
            TraceListenerCollection ls = Listeners;
            for (int i = 0; i < ls.Count; i++)
            {
                TraceListener l = ls[i];
                if (l != null) l.Close();
            }
        }

        public static void Assert(bool condition)
        {
            if (!condition) Fail("");
        }

        public static void Assert(bool condition, string message)
        {
            if (!condition) Fail(message);
        }

        public static void Assert(bool condition, string message, string detailMessage)
        {
            if (!condition) Fail(message, detailMessage);
        }

        public static void Fail(string message)
        {
            Fail(message, "");
        }

        public static void Fail(string message, string detailMessage)
        {
            TraceListenerCollection ls = Listeners;
            for (int i = 0; i < ls.Count; i++)
            {
                TraceListener l = ls[i];
                if (l != null) l.Fail(message, detailMessage);
            }
        }
    }
}
