// Lamella managed corlib (from scratch). -- System.Enum
namespace System
{
    public abstract class Enum : ValueType
    {
        [Lamella.Runtime.RuntimeProvided] public bool HasFlag(Enum flag) { return false; }
        [Lamella.Runtime.RuntimeProvided] public string ToString(string format) { return null; }

        [Lamella.Runtime.RuntimeProvided] public static object Parse(Type enumType, string value) { return null; }
        [Lamella.Runtime.RuntimeProvided] public static bool IsDefined(Type enumType, object value) { return false; }
        [Lamella.Runtime.RuntimeProvided] public static string GetName(Type enumType, object value) { return null; }
        [Lamella.Runtime.RuntimeProvided] public static string[] GetNames(Type enumType) { return null; }
        [Lamella.Runtime.RuntimeProvided] public static Array GetValues(Type enumType) { return null; }
        [Lamella.Runtime.RuntimeProvided] public static string Format(Type enumType, object value, string format) { return null; }
    }
}
