// Lamella managed corlib (from scratch). -- System.Convert
namespace System
{
    public sealed class Convert
    {
        public static int ToInt32(string value) { return Int32.Parse(value); }
        public static string ToString(bool value) { return value ? "True" : "False"; }
    }
}
