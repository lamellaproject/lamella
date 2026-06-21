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
    }
}
