// Lamella managed corlib (from scratch). -- System.Runtime.CompilerServices.InternalsVisibleToAttribute
#if LAMELLA_SURFACE_NETFX_2_0
namespace System.Runtime.CompilerServices
{
    [System.AttributeUsage(System.AttributeTargets.Assembly, AllowMultiple = true, Inherited = false)]
    public sealed class InternalsVisibleToAttribute : System.Attribute
    {
        private readonly string _assemblyName;
        private bool _allInternalsVisible;

        public InternalsVisibleToAttribute(string assemblyName)
        {
            _assemblyName = assemblyName;
        }

        public string AssemblyName { get { return _assemblyName; } }

        public bool AllInternalsVisible
        {
            get { return _allInternalsVisible; }
            set { _allInternalsVisible = value; }
        }
    }
}
#endif
