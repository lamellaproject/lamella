// Lamella managed corlib (from scratch). -- System.Diagnostics.DebuggerTypeProxyAttribute
#if LAMELLA_SURFACE_NETFX_2_0
namespace System.Diagnostics
{
    /// <summary>Names a proxy type a debugger displays in place of the marked type.</summary>
    [AttributeUsage(AttributeTargets.Assembly | AttributeTargets.Class | AttributeTargets.Struct, AllowMultiple = true, Inherited = true)]
    public sealed class DebuggerTypeProxyAttribute : Attribute
    {
        private string _proxyTypeName;
        private Type _target;
        private string _targetTypeName;

        /// <summary>Initializes the attribute with the proxy type.</summary>
        public DebuggerTypeProxyAttribute(Type type)
        {
            _proxyTypeName = (object)type == null ? null : type.AssemblyQualifiedName;
        }

        /// <summary>Initializes the attribute with the assembly-qualified proxy type name.</summary>
        public DebuggerTypeProxyAttribute(string typeName) { _proxyTypeName = typeName; }

        /// <summary>The assembly-qualified name of the proxy type.</summary>
        public string ProxyTypeName { get { return _proxyTypeName; } }

        /// <summary>The type this attribute applies to when applied at assembly scope.</summary>
        public Type Target { get { return _target; } set { _target = value; } }

        /// <summary>The name of the type this attribute applies to.</summary>
        public string TargetTypeName { get { return _targetTypeName; } set { _targetTypeName = value; } }
    }
}
#endif
