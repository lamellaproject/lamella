// Lamella managed corlib (from scratch). -- System.Diagnostics.DefaultTraceListener
namespace System.Diagnostics
{
    public class DefaultTraceListener : TraceListener
    {
        public DefaultTraceListener() : base("Default")
        {
        }

        [Lamella.Runtime.RuntimeProvided] private static void DebugWrite(string message) { }

        public override void Write(string message)
        {
            if (NeedIndent) WriteIndent();
            DebugWrite(message);
        }

        public override void WriteLine(string message)
        {
            if (NeedIndent) WriteIndent();
            DebugWrite(message);
            DebugWrite("\n");
            NeedIndent = true;
        }
    }
}
