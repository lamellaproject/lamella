// Lamella managed corlib (from scratch). -- System.NumberText (internal)
namespace System
{
    internal sealed class NumberText
    {
        private NumberText() { }

        internal static bool IsPad(char c)
        {
            return (c >= (char)0x09 && c <= (char)0x0D) || c == (char)0x20;
        }
    }
}
