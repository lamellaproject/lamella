// Lamella managed corlib (from scratch). -- System.Diagnostics.DebuggerStepperBoundaryAttribute
#if LAMELLA_SURFACE_NETFX_2_0
namespace System.Diagnostics
{
    /// <summary>Marks a boundary at which stepping resumes running the program.</summary>
    [AttributeUsage(AttributeTargets.Constructor | AttributeTargets.Method, Inherited = false)]
    public sealed class DebuggerStepperBoundaryAttribute : Attribute
    {
        /// <summary>Initializes the attribute.</summary>
        public DebuggerStepperBoundaryAttribute() { }
    }
}
#endif
