// Lamella managed corlib (from scratch). -- System.Reflection.Assembly
namespace System.Reflection
{
    public class Assembly
    {
        protected Assembly() { }

        [Lamella.Runtime.RuntimeProvided] public System.Type GetType(string name) { return null; }

        public string FullName
        {
            [Lamella.Runtime.RuntimeProvided] get { return null; }
        }

        [Lamella.Runtime.RuntimeProvided] public System.Type[] GetTypes() { return null; }

        [Lamella.Runtime.RuntimeProvided] private static bool HandleEquals(Assembly left, Assembly right) { return false; }
        public static bool operator ==(Assembly left, Assembly right) { return HandleEquals(left, right); }

        public static bool operator !=(Assembly left, Assembly right) { return !HandleEquals(left, right); }
    }
}
