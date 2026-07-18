// Lamella managed corlib (from scratch). -- System.Reflection.AssemblyVersionAttribute
namespace System.Reflection
{
    [System.AttributeUsage(System.AttributeTargets.Assembly, Inherited = false)]
    public sealed class AssemblyVersionAttribute : System.Attribute
    {
        private readonly string _version;

        public AssemblyVersionAttribute(string version)
        {
            _version = version;
        }

        public string Version
        {
            get { return _version; }
        }
    }
}
