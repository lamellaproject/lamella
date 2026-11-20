// Lamella managed corlib (from scratch). -- System.Runtime.CompilerServices.MethodImplAttribute
namespace System.Runtime.CompilerServices
{
    /// <summary>Specifies how a method is implemented. A compiler writes its options into the method's implementation
    /// flags.</summary>
    [AttributeUsage(AttributeTargets.Constructor | AttributeTargets.Method, Inherited = false)]
    public sealed class MethodImplAttribute : Attribute
    {
        private MethodImplOptions _value;

        /// <summary>The kind of code the method's body is.</summary>
        public MethodCodeType MethodCodeType;

        /// <summary>Specifies the method's implementation by the options given.</summary>
        public MethodImplAttribute(MethodImplOptions methodImplOptions) { _value = methodImplOptions; }

        /// <summary>Specifies the method's implementation by an options value given as a 16-bit integer.</summary>
        public MethodImplAttribute(short value) { _value = (MethodImplOptions)value; }

        /// <summary>Specifies no implementation options.</summary>
        public MethodImplAttribute() { }

        /// <summary>The options the method's implementation was specified with.</summary>
        public MethodImplOptions Value { get { return _value; } }
    }
}
