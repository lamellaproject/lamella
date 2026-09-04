// Lamella managed corlib (from scratch). -- System.Diagnostics.DebuggerStepThroughAttribute
namespace System.Diagnostics
{
    /// <summary>Tells the debugger to step through the marked code rather than into it.</summary>
    [AttributeUsage(AttributeTargets.Class | AttributeTargets.Struct | AttributeTargets.Constructor | AttributeTargets.Method, Inherited = false)]
    public sealed class DebuggerStepThroughAttribute : Attribute
    {
        /// <summary>Initializes the attribute.</summary>
        public DebuggerStepThroughAttribute() { }
    }
}
