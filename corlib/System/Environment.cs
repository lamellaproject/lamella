// Lamella managed corlib (from scratch). -- System.Environment
namespace System
{
    public sealed class Environment
    {
        private Environment() { }

        public static string NewLine { get { return "\r\n"; } }

        public static int TickCount
        {
            [Lamella.Runtime.RuntimeProvided] get { return 0; }
        }

        public static int ProcessorCount
        {
            [Lamella.Runtime.RuntimeProvided] get { return 0; }
        }

        [Lamella.Runtime.RuntimeProvided] public static string GetEnvironmentVariable(string variable) { return null; }
    }
}
