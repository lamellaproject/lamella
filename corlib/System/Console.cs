// Lamella managed corlib (from scratch). -- System.Console
namespace System
{
#if LAMELLA_SURFACE_NETFX_2_0
    public static class Console
#else
    public sealed class Console
#endif
    {
#if !LAMELLA_SURFACE_NETFX_2_0
        private Console() { }
#endif

        [Lamella.Runtime.RuntimeProvided] public static void WriteLine() { }
        [Lamella.Runtime.RuntimeProvided] public static void WriteLine(int value) { }
        [Lamella.Runtime.RuntimeProvided] public static void WriteLine(string value) { }
        [Lamella.Runtime.RuntimeProvided] public static void WriteLine(bool value) { }
        [Lamella.Runtime.RuntimeProvided] public static void WriteLine(char value) { }
        [Lamella.Runtime.RuntimeProvided] public static void WriteLine(long value) { }
        [Lamella.Runtime.RuntimeProvided] public static void WriteLine(uint value) { }
        [Lamella.Runtime.RuntimeProvided] public static void WriteLine(ulong value) { }

        public static void WriteLine(object value)
        {
            if ((object)value == null) WriteLine("");
            else WriteLine(value.ToString());
        }

#if LAMELLA_SURFACE_FLOAT
        public static void WriteLine(double value) { WriteLine(value.ToString()); }
#endif

#if LAMELLA_SURFACE_DECIMAL
        public static void WriteLine(decimal value) { WriteLine(value.ToString()); }
#endif

        [Lamella.Runtime.RuntimeProvided] public static void Write(string value) { }
        [Lamella.Runtime.RuntimeProvided] public static void Write(char value) { }
        [Lamella.Runtime.RuntimeProvided] public static void Write(int value) { }
        [Lamella.Runtime.RuntimeProvided] public static void Write(bool value) { }
        [Lamella.Runtime.RuntimeProvided] public static void Write(long value) { }
        [Lamella.Runtime.RuntimeProvided] public static void Write(uint value) { }
        [Lamella.Runtime.RuntimeProvided] public static void Write(ulong value) { }

        public static void Write(object value)
        {
            if ((object)value != null) Write(value.ToString());
        }

#if LAMELLA_SURFACE_FLOAT
        public static void Write(double value) { Write(value.ToString()); }
#endif
#if LAMELLA_SURFACE_DECIMAL
        public static void Write(decimal value) { Write(value.ToString()); }
#endif

        private static System.IO.TextWriter _out;
        public static System.IO.TextWriter Out
        {
            get
            {
                if ((object)_out == null) _out = new ConsoleTextWriter();
                return _out;
            }
        }

        private sealed class ConsoleTextWriter : System.IO.TextWriter
        {
            public override void Write(char value)
            {
                Console.Write(value);
            }

            public override void Write(string value)
            {
                if (value == null) return;
                Console.Write(value);
            }

            public override void WriteLine(string value)
            {
                Console.WriteLine(value);
            }

            public override void WriteLine()
            {
                Console.WriteLine("");
            }
        }
    }
}
