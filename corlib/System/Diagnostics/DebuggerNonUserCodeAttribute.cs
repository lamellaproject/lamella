// Lamella managed corlib (from scratch). -- System.Diagnostics.DebuggerNonUserCodeAttribute
#if LAMELLA_SURFACE_NETFX_2_0
namespace System.Diagnostics
{
    /// <summary>Marks code that is not user-written, so a debugger can step over it.</summary>
    [AttributeUsage(AttributeTargets.Class | AttributeTargets.Struct | AttributeTargets.Constructor | AttributeTargets.Method | AttributeTargets.Property, Inherited = false)]
    public sealed class DebuggerNonUserCodeAttribute : Attribute
    {
        /// <summary>Initializes the attribute.</summary>
        public DebuggerNonUserCodeAttribute() { }
    }
}
#endif
