// Lamella managed corlib (from scratch). -- System.Reflection.AssemblyFileVersionAttribute
namespace System.Reflection
{
    [System.AttributeUsage(System.AttributeTargets.Assembly, Inherited = false)]
    public sealed class AssemblyFileVersionAttribute : System.Attribute
    {
        public AssemblyFileVersionAttribute(string version)
        {
        }
    }
}
