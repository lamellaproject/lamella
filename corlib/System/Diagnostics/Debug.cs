// Lamella managed corlib (from scratch). -- System.Diagnostics.Debug
namespace System.Diagnostics
{
#if LAMELLA_SURFACE_NETFX_4_0
    public static class Debug
#else
    public sealed class Debug
#endif
    {
#if !LAMELLA_SURFACE_NETFX_4_0
        private Debug() { }
#endif

        public static int IndentLevel
        {
            get { return Trace.IndentLevel; }
            set { Trace.IndentLevel = value; }
        }

        public static int IndentSize
        {
            get { return Trace.IndentSize; }
            set { Trace.IndentSize = value; }
        }

        public static void Indent()
        {
            Trace.Indent();
        }

        public static void Unindent()
        {
            Trace.Unindent();
        }

        public static void Write(string message)
        {
            Trace.Write(message);
        }

        public static void Write(object value)
        {
            Trace.Write(value);
        }

        public static void WriteLine(string message)
        {
            Trace.WriteLine(message);
        }

        public static void WriteLine(object value)
        {
            Trace.WriteLine(value);
        }

#if LAMELLA_SURFACE_NETFX_2_0
        public static void Print(string message)
        {
            Trace.WriteLine(message);
        }
#endif

        public static void WriteIf(bool condition, string message)
        {
            Trace.WriteIf(condition, message);
        }

        public static void WriteIf(bool condition, object value)
        {
            Trace.WriteIf(condition, value);
        }

        public static void WriteLineIf(bool condition, string message)
        {
            Trace.WriteLineIf(condition, message);
        }

        public static void WriteLineIf(bool condition, object value)
        {
            Trace.WriteLineIf(condition, value);
        }

        public static void Flush()
        {
            Trace.Flush();
        }

        public static void Close()
        {
            Trace.Close();
        }

        public static void Assert(bool condition)
        {
            Trace.Assert(condition);
        }

        public static void Assert(bool condition, string message)
        {
            Trace.Assert(condition, message);
        }

        public static void Assert(bool condition, string message, string detailMessage)
        {
            Trace.Assert(condition, message, detailMessage);
        }

        public static void Fail(string message)
        {
            Trace.Fail(message);
        }

        public static void Fail(string message, string detailMessage)
        {
            Trace.Fail(message, detailMessage);
        }
    }
}
