// Lamella managed corlib (from scratch). -- System.Text.StringBuilder
namespace System.Text
{
    public sealed class StringBuilder
    {
        [Lamella.Runtime.RuntimeProvided] public StringBuilder() { }
        [Lamella.Runtime.RuntimeProvided] public StringBuilder(int capacity) { }
        [Lamella.Runtime.RuntimeProvided] public StringBuilder(string value) { }

        public int Length { [Lamella.Runtime.RuntimeProvided] get { return 0; } }

        [Lamella.Runtime.RuntimeProvided] public StringBuilder Append(string value) { return null; }
        [Lamella.Runtime.RuntimeProvided] public StringBuilder Append(char value) { return null; }
        [Lamella.Runtime.RuntimeProvided] public StringBuilder Append(int value) { return null; }

        [Lamella.Runtime.RuntimeProvided] public StringBuilder Insert(int index, string value) { return null; }
        [Lamella.Runtime.RuntimeProvided] public StringBuilder Remove(int startIndex, int length) { return null; }
        [Lamella.Runtime.RuntimeProvided] public StringBuilder Replace(char oldChar, char newChar) { return null; }

        [Lamella.Runtime.RuntimeProvided] public override string ToString() { return null; }
    }
}
