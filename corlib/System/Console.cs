// Lamella managed corlib (from scratch). -- System.Console
namespace System
{
    public sealed class Console
    {
        [Lamella.Runtime.RuntimeProvided] public static void WriteLine(int value) { }
        [Lamella.Runtime.RuntimeProvided] public static void WriteLine(string value) { }
        [Lamella.Runtime.RuntimeProvided] public static void WriteLine(bool value) { }
        [Lamella.Runtime.RuntimeProvided] public static void WriteLine(char value) { }
        [Lamella.Runtime.RuntimeProvided] public static void WriteLine(long value) { }
        [Lamella.Runtime.RuntimeProvided] public static void WriteLine(uint value) { }
        [Lamella.Runtime.RuntimeProvided] public static void WriteLine(ulong value) { }

        public static void WriteLine(decimal value) { WriteLine(value.ToString()); }

        [Lamella.Runtime.RuntimeProvided] public static void Write(string value) { }
        [Lamella.Runtime.RuntimeProvided] public static void Write(char value) { }
        [Lamella.Runtime.RuntimeProvided] public static void Write(int value) { }
        public static void Write(decimal value) { Write(value.ToString()); }

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
