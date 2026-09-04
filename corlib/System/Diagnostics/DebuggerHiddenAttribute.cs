// Lamella managed corlib (from scratch). -- System.Diagnostics.DebuggerHiddenAttribute
namespace System.Diagnostics
{
    /// <summary>Tells the debugger to step through the marked code rather than into it.</summary>
    [AttributeUsage(AttributeTargets.Constructor | AttributeTargets.Method | AttributeTargets.Property, Inherited = false)]
    public sealed class DebuggerHiddenAttribute : Attribute
    {
        /// <summary>Initializes the attribute.</summary>
        public DebuggerHiddenAttribute() { }
    }
}
