// Lamella managed corlib (from scratch). -- System.Diagnostics.DebuggerDisplayAttribute
#if LAMELLA_SURFACE_NETFX_2_0
namespace System.Diagnostics
{
    /// <summary>Determines how a debugger displays instances of the marked type.</summary>
    [AttributeUsage(AttributeTargets.Assembly | AttributeTargets.Class | AttributeTargets.Struct
        | AttributeTargets.Enum | AttributeTargets.Property | AttributeTargets.Field
        | AttributeTargets.Delegate, AllowMultiple = true, Inherited = true)]
    public sealed class DebuggerDisplayAttribute : Attribute
    {
        private string _value;
        private string _name;
        private string _type;
        private Type _target;
        private string _targetTypeName;

        /// <summary>Initializes the attribute with the display format string.</summary>
        public DebuggerDisplayAttribute(string value) { _value = value; }

        /// <summary>The display format string.</summary>
        public string Value { get { return _value; } }

        /// <summary>The name to display in the debugger's value column heading.</summary>
        public string Name { get { return _name; } set { _name = value; } }

        /// <summary>The type to display in the debugger's type column.</summary>
        public string Type { get { return _type; } set { _type = value; } }

        /// <summary>The type this attribute applies to when applied at assembly scope.</summary>
        public Type Target { get { return _target; } set { _target = value; } }

        /// <summary>The name of the type this attribute applies to.</summary>
        public string TargetTypeName { get { return _targetTypeName; } set { _targetTypeName = value; } }
    }
}
#endif
